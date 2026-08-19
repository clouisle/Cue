mod config;
mod runner;
mod term;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Parser, Subcommand};
use tokio::sync::mpsc;

use config::Config;
use runner::{ServiceEvent, SharedPids, Shutdown, signal_all};

#[derive(Parser)]
#[command(
    name = "yun-dev-manage",
    version,
    about = "One-command dev service orchestrator driven by .yun-dev.json, with docker-compose style logs"
)]
struct Cli {
    /// Path to the config file (default: nearest .yun-dev.json found upward from cwd)
    #[arg(short, long, global = true)]
    file: Option<PathBuf>,

    /// Disable colored output
    #[arg(long, global = true)]
    no_color: bool,

    /// Seconds to wait for graceful shutdown before SIGKILL (0 = immediate)
    #[arg(long, global = true, default_value_t = 10)]
    stop_timeout: u64,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start all services and stream their logs (default command)
    Up,
    /// Print the resolved configuration as JSON
    Config,
    /// Validate the configuration only
    Validate,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let cfg_path = match &cli.file {
        Some(p) => p.clone(),
        None => {
            let cwd = std::env::current_dir().expect("cannot determine current directory");
            match config::discover(&cwd) {
                Some(p) => p,
                None => {
                    eprintln!(
                        "error: no {} found in this directory or any parent; use --file",
                        config::CONFIG_FILE_NAME
                    );
                    std::process::exit(1);
                }
            }
        }
    };

    let cfg = match Config::load(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let code = match cli.command.unwrap_or(Command::Up) {
        Command::Up => up(&cfg, &cfg_path, cli.stop_timeout, cli.no_color).await,
        Command::Config => {
            println!("{}", serde_json::to_string_pretty(&cfg).expect("config serializes"));
            0
        }
        Command::Validate => {
            println!("OK: {}", cfg_path.display());
            0
        }
    };
    std::process::exit(code);
}

async fn up(cfg: &Config, cfg_path: &Path, stop_timeout: u64, no_color: bool) -> i32 {
    if cfg.services.is_empty() {
        eprintln!("error: no services defined in {}", cfg_path.display());
        return 1;
    }

    let colors_on = use_color(no_color);
    let names: Vec<&str> = cfg.services.keys().map(|s| s.as_str()).collect();
    runner::print_status(&term::dim(&format!(
        "Using {} · {} service(s): {}",
        cfg_path.display(),
        names.len(),
        names.join(", ")
    )));

    let width = cfg.max_name_width();
    let base = cfg_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let shutdown = Arc::new(Shutdown::new());
    let pids: SharedPids = Arc::new(Mutex::new(Vec::new()));
    let (tx, mut rx) = mpsc::channel::<ServiceEvent>(64);

    let mut remaining = cfg.services.len();
    for (i, (name, svc)) in cfg.services.iter().enumerate() {
        let name = name.clone();
        let svc = svc.clone();
        let base = base.clone();
        let shutdown = shutdown.clone();
        let tx = tx.clone();
        let color = if colors_on {
            term::color_for(i).to_string()
        } else {
            String::new()
        };
        tokio::spawn(async move {
            runner::run_service(name, svc, base, color, width, shutdown, tx).await;
        });
    }
    drop(tx);

    let mut any_failed = false;
    let mut shutting_down = false;
    loop {
        tokio::select! {
            ev = rx.recv() => {
                match ev {
                    Some(ServiceEvent::Started { pid }) => {
                        pids.lock().unwrap().push(pid);
                    }
                    Some(ServiceEvent::Exited { code }) => {
                        remaining -= 1;
                        // 仅显式非零退出码记为失败；停机时被信号终止不计。
                        if matches!(code, Some(c) if c != 0) {
                            any_failed = true;
                        }
                    }
                    Some(ServiceEvent::Failed) => {
                        remaining -= 1;
                        any_failed = true;
                    }
                    None => break,
                }
                if remaining == 0 && !shutting_down {
                    break;
                }
            }
            _ = ctrl_c_or_term() => {
                if shutting_down {
                    // 第二次：立即强制。
                    force_kill(&pids);
                    runner::print_status(&term::dim("Forced shutdown."));
                    break;
                }
                shutting_down = true;
                runner::print_status(&term::dim(
                    "Gracefully stopping... (press Ctrl+C again to force)",
                ));
                shutdown.trigger();
                if stop_timeout > 0 {
                    let pids = pids.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(stop_timeout)).await;
                        force_kill(&pids);
                        runner::print_status(&term::dim("Shutdown timeout reached; forced."));
                    });
                }
                signal_all(&pids.lock().unwrap(), graceful_signal());
                if stop_timeout == 0 {
                    force_kill(&pids);
                }
            }
        }
    }

    if any_failed {
        1
    } else {
        0
    }
}

/// SIGTERM（unix）或 taskkill 语义（其他平台）。
#[cfg(unix)]
fn graceful_signal() -> i32 {
    libc::SIGTERM
}

#[cfg(not(unix))]
fn graceful_signal() -> i32 {
    0
}

#[cfg(unix)]
fn force_kill(pids: &SharedPids) {
    signal_all(&pids.lock().unwrap(), libc::SIGKILL);
}

#[cfg(not(unix))]
fn force_kill(pids: &SharedPids) {
    signal_all(&pids.lock().unwrap(), 0);
}

/// Ctrl+C 或 SIGTERM（unix）触发优雅停机；二者路径一致。
#[cfg(unix)]
async fn ctrl_c_or_term() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(not(unix))]
async fn ctrl_c_or_term() {
    let _ = tokio::signal::ctrl_c().await;
}

/// 颜色开关：非 TTY、NO_COLOR 或 --no-color 时关闭。
fn use_color(no_color: bool) -> bool {
    use std::io::IsTerminal;
    !no_color && std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

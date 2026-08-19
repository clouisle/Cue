mod config;
mod runner;
mod session;
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
    Up(UpArgs),
    /// Stop the background session (SIGTERM -> timeout -> SIGKILL)
    Down,
    /// Print or follow background session logs
    Logs(LogsArgs),
    /// List background session services and their status
    Ps,
    /// Print the resolved configuration as JSON
    Config,
    /// Validate the configuration only
    Validate,
}

#[derive(clap::Args)]
struct UpArgs {
    /// Run in the background; manage with ps / logs / down
    #[arg(short, long)]
    detach: bool,
}

#[derive(clap::Args)]
struct LogsArgs {
    /// Follow new output (no short flag: -f is the global --file)
    #[arg(long)]
    follow: bool,
    /// Show only the last N lines (0 = only new output when following)
    #[arg(long)]
    tail: Option<usize>,
    /// Service name(s) to show (default: all)
    #[arg(value_name = "SERVICE")]
    services: Vec<String>,
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

    // Down / Logs / Ps 只依赖配置文件路径定位会话，不解析配置内容，
    // 避免配置损坏后无法停止已后台运行的服务。
    let command = cli.command.unwrap_or(Command::Up(UpArgs { detach: false }));
    let code = match command {
        Command::Up(args) => {
            let cfg = match Config::load(&cfg_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = check_no_running_session(&cfg_path) {
                eprintln!("error: {e}");
                1
            } else if args.detach {
                up_detached(&cfg, &cfg_path).await
            } else {
                up(&cfg, &cfg_path, cli.stop_timeout, cli.no_color).await
            }
        }
        Command::Down => down_cmd(&cfg_path, cli.stop_timeout).await,
        Command::Logs(args) => {
            logs_cmd(&cfg_path, args.follow, args.tail, args.services, cli.no_color).await
        }
        Command::Ps => ps_cmd(&cfg_path).await,
        Command::Config => {
            let cfg = match Config::load(&cfg_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            println!("{}", serde_json::to_string_pretty(&cfg).expect("config serializes"));
            0
        }
        Command::Validate => {
            if let Err(e) = Config::load(&cfg_path) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
            println!("OK: {}", cfg_path.display());
            0
        }
    };
    std::process::exit(code);
}

/// 前后台互斥：存在存活的后台服务时拒绝再次启动（前台 up 或 up -d）。
fn check_no_running_session(cfg_path: &Path) -> Result<(), String> {
    if let Some(s) = session::load(cfg_path)
        && s.is_any_running()
    {
        return Err(format!(
            "background session is running ({} service(s)); run 'yun-dev-manage down' first",
            s.services.len()
        ));
    }
    Ok(())
}

/// 后台启动全部服务，写会话状态后立即退出；工具退出后服务继续运行。
async fn up_detached(cfg: &Config, cfg_path: &Path) -> i32 {
    if cfg.services.is_empty() {
        eprintln!("error: no services defined in {}", cfg_path.display());
        return 1;
    }

    let base = cfg_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let dir = session::session_dir(cfg_path);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("error: cannot create session dir {}: {e}", dir.display());
        return 1;
    }

    let mut session_services = Vec::new();
    let mut failures = 0;
    for (name, svc) in &cfg.services {
        let log = dir.join(format!("{name}.log"));
        match runner::spawn_detached(name, svc, &base, &log) {
            Ok(pid) => session_services.push(session::SessionService {
                name: name.clone(),
                pid,
                log,
            }),
            Err(e) => {
                eprintln!("error: {e}");
                failures += 1;
            }
        }
    }

    let record = session::Session {
        version: session::STATE_VERSION,
        config_path: cfg_path.to_path_buf(),
        services: session_services,
    };
    if let Err(e) = session::save(cfg_path, &record) {
        // 状态写失败：杀掉已起服务，避免孤儿进程。
        for s in &record.services {
            if session::is_alive(s.pid) {
                runner::signal_group(s.pid, sigkill());
            }
        }
        eprintln!("error: {e}");
        return 1;
    }

    let names: Vec<&str> = cfg.services.keys().map(|s| s.as_str()).collect();
    runner::print_status(&format!(
        "started {} service(s) in background: {}",
        record.services.len(),
        names.join(", ")
    ));
    runner::print_status(&term::dim(
        "logs: yun-dev-manage logs --follow · status: yun-dev-manage ps · stop: yun-dev-manage down",
    ));
    if failures > 0 {
        1
    } else {
        0
    }
}

/// 停止后台会话：SIGTERM → 等 stop-timeout → SIGKILL → 清状态。
async fn down_cmd(cfg_path: &Path, stop_timeout: u64) -> i32 {
    let Some(record) = session::load(cfg_path) else {
        eprintln!("error: no background session for {}", cfg_path.display());
        return 1;
    };
    let running: Vec<&session::SessionService> =
        record.services.iter().filter(|s| session::is_alive(s.pid)).collect();

    if running.is_empty() {
        runner::print_status("no running services; clearing session state");
        session::clear(cfg_path);
        return 0;
    }

    for s in &running {
        runner::print_status(&format!("stopping {} (pid {})", s.name, s.pid));
        runner::signal_group(s.pid, graceful_signal());
    }
    if stop_timeout > 0 {
        record.wait_all_exited(Duration::from_secs(stop_timeout));
    }

    let mut killed = 0;
    for s in &running {
        if session::is_alive(s.pid) {
            runner::signal_group(s.pid, sigkill());
            killed += 1;
        }
    }
    if killed > 0 {
        record.wait_all_exited(Duration::from_secs(2));
    }

    session::clear(cfg_path);
    runner::print_status(&format!(
        "stopped {} service(s); session state cleared",
        running.len()
    ));
    0
}

/// 打印后台会话状态表。
async fn ps_cmd(cfg_path: &Path) -> i32 {
    let Some(record) = session::load(cfg_path) else {
        runner::print_status(&format!("no background session for {}", cfg_path.display()));
        return 1;
    };
    let name_w = record
        .services
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(0)
        .max(4);
    runner::print_status(&format!(
        "{:<name_w$} {:>8}  {:<8} {}",
        "NAME", "PID", "STATUS", "LOG"
    ));
    for s in &record.services {
        let status = if session::is_alive(s.pid) { "running" } else { "exited" };
        runner::print_status(&format!(
            "{:<name_w$} {:>8}  {:<8} {}",
            s.name,
            s.pid,
            status,
            s.log.display()
        ));
    }
    0
}

/// 打印（或跟随）后台会话日志，带服务名前缀。
async fn logs_cmd(
    cfg_path: &Path,
    follow: bool,
    tail: Option<usize>,
    services: Vec<String>,
    no_color: bool,
) -> i32 {
    let Some(record) = session::load(cfg_path) else {
        eprintln!("error: no background session for {}", cfg_path.display());
        return 1;
    };
    let colors_on = use_color(no_color);
    let width = record.services.iter().map(|s| s.name.len()).max().unwrap_or(0);

    let mut handles = Vec::new();
    for (i, s) in record.services.iter().enumerate() {
        if !services.is_empty() && !services.contains(&s.name) {
            continue;
        }
        let color = if colors_on {
            term::color_for(i).to_string()
        } else {
            String::new()
        };
        let name = s.name.clone();
        let log = s.log.clone();
        handles.push(tokio::spawn(async move {
            stream_log(&name, &log, follow, tail, width, &color).await;
        }));
    }
    if handles.is_empty() {
        eprintln!("error: no such service(s): {}", services.join(", "));
        return 1;
    }
    for h in handles {
        let _ = h.await;
    }
    0
}

/// 单文件日志流：完整行即时打印，无换行残留留待下轮；轮询 200ms。
async fn stream_log(
    name: &str,
    path: &Path,
    follow: bool,
    tail: Option<usize>,
    width: usize,
    color: &str,
) {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let mut pos = if let Some(n) = tail {
        tail_offset(path, n).await
    } else {
        0
    };

    loop {
        if let Ok(mut f) = tokio::fs::File::open(path).await
            && let Ok(len) = f.metadata().await.map(|m| m.len())
        {
            if pos > len {
                pos = 0; // 文件被重建
            }
            if pos < len {
                let _ = f.seek(std::io::SeekFrom::Start(pos)).await;
                let mut data = Vec::new();
                if f.read_to_end(&mut data).await.is_ok() {
                    let text = String::from_utf8_lossy(&data);
                    let complete_end = text.rfind('\n').map(|i| i + 1).unwrap_or(0);
                    for line in text[..complete_end].lines() {
                        runner::print_line(name, color, width, &runner::Line {
                            text: line.to_string(),
                        });
                    }
                    pos += complete_end as u64;
                    if !follow && pos < len {
                        let rest = &text[complete_end..];
                        if !rest.is_empty() {
                            runner::print_line(name, color, width, &runner::Line {
                                text: rest.to_string(),
                            });
                        }
                        pos = len;
                    }
                }
            }
        }
        if !follow {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// 定位倒数第 N 行的起始字节偏移（整读文件，dev 日志量级可接受）。
async fn tail_offset(path: &Path, n: usize) -> u64 {
    let Ok(data) = tokio::fs::read(path).await else {
        return 0;
    };
    if n == 0 {
        return data.len() as u64;
    }
    let mut count = 0usize;
    let mut idx = data.len();
    while idx > 0 {
        idx -= 1;
        if data[idx] == b'\n' {
            count += 1;
            if count == n {
                return idx as u64 + 1;
            }
        }
    }
    0
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

#[cfg(unix)]
fn sigkill() -> i32 {
    libc::SIGKILL
}

#[cfg(not(unix))]
fn graceful_signal() -> i32 {
    0
}

#[cfg(not(unix))]
fn sigkill() -> i32 {
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

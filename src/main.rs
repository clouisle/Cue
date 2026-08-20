mod config;
mod runner;
mod session;
mod term;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Parser, Subcommand};
use tokio::sync::{mpsc, Notify};

use config::Config;
use runner::{ServiceEvent, SharedPids, Shutdown, signal_all};

#[derive(Parser)]
#[command(
    name = "cue",
    version,
    about = "One-command dev service orchestrator driven by .cue.json, with docker-compose style logs"
)]
struct Cli {
    /// Path to the config file (default: nearest .cue.json found upward from cwd)
    #[arg(long, global = true)]
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
    /// Start selected services and stream their logs (default: all)
    Up(UpArgs),
    /// Restart background service(s)
    Restart(RestartArgs),
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
    /// Service name(s) to start, including their dependencies (default: all)
    #[arg(value_name = "SERVICE")]
    services: Vec<String>,
}

#[derive(clap::Args)]
struct RestartArgs {
    /// Background service name(s) to restart (default: all)
    #[arg(value_name = "SERVICE")]
    services: Vec<String>,
}

#[derive(clap::Args)]
struct LogsArgs {
    /// Follow new output
    #[arg(short = 'f', long)]
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
    let command = cli.command.unwrap_or(Command::Up(UpArgs {
        detach: false,
        services: Vec::new(),
    }));
    let code = match command {
        Command::Up(args) => {
            let cfg = match Config::load(&cfg_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            match cfg.selected_services(&args.services) {
                Ok(selected) => {
                    if let Err(e) = check_no_running_session(&cfg_path) {
                        eprintln!("error: {e}");
                        1
                    } else if args.detach {
                        up_detached(&cfg, &cfg_path, &selected).await
                    } else {
                        up(&cfg, &cfg_path, &selected, cli.stop_timeout, cli.no_color).await
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    1
                }
            }
        }
        Command::Restart(args) => {
            let cfg = match Config::load(&cfg_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            restart_cmd(&cfg, &cfg_path, args.services, cli.stop_timeout).await
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
            "background session is running ({} service(s)); run 'cue down' first",
            s.services.len()
        ));
    }
    Ok(())
}

/// 后台启动全部服务：按依赖波次 spawn，同步等待各层就绪后写状态退出。
async fn up_detached(cfg: &Config, cfg_path: &Path, selected: &BTreeSet<String>) -> i32 {
    if selected.is_empty() {
        eprintln!("error: no services defined in {}", cfg_path.display());
        return 1;
    }

    let levels = match cfg.levels() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let required = cfg.required_conditions_for(selected);

    let base = cfg_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let dir = session::session_dir(cfg_path);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("error: cannot create session dir {}: {e}", dir.display());
        return 1;
    }

    let mut session_services = Vec::new();
    let mut children: HashMap<String, std::process::Child> = HashMap::new();
    let mut failed: HashSet<String> = HashSet::new();
    let mut failures = 0;

    for level in &levels {
        // 依赖失败检查 + spawn 本层。
        let mut wave = Vec::new();
        for name in level {
            if !selected.contains(name) {
                continue;
            }
            let svc = &cfg.services[name];
            if cfg.deps_of(name).keys().any(|d| failed.contains(d)) {
                eprintln!("error: service '{name}' skipped: dependency failed to start");
                failed.insert(name.clone());
                failures += 1;
                continue;
            }
            let log = dir.join(format!("{name}.log"));
            match runner::spawn_detached(name, svc, &base, &log) {
                Ok(child) => {
                    let pid = child.id();
                    children.insert(name.clone(), child);
                    session_services.push(session::SessionService {
                        name: name.clone(),
                        pid,
                        log,
                    });
                    wave.push(name.clone());
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    failed.insert(name.clone());
                    failures += 1;
                }
            }
        }
        // 同步等待本层就绪。
        for name in &wave {
            let cond = required.get(name).copied().unwrap_or(config::DepCondition::Started);
            let ok = match cond {
                config::DepCondition::Started => true,
                config::DepCondition::Healthy => {
                    let hc = cfg.services[name].healthcheck.as_ref().expect("validated");
                    wait_healthy_sync(hc)
                }
                config::DepCondition::Completed => children
                    .get_mut(name)
                    .map(wait_completed_sync)
                    .unwrap_or(false),
            };
            if !ok {
                eprintln!("error: service '{name}' not ready; dependents skipped");
                failed.insert(name.clone());
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

    let names: Vec<&str> = selected.iter().map(String::as_str).collect();
    runner::print_status(&format!(
        "started {} service(s) in background: {}",
        record.services.len(),
        names.join(", ")
    ));
    runner::print_status(&term::dim(
        "logs: cue logs -f · status: cue ps · stop: cue down",
    ));
    if failures > 0 {
        1
    } else {
        0
    }
}

/// 同步健康检查轮询（后台模式阻塞式）。
fn wait_healthy_sync(hc: &config::Healthcheck) -> bool {
    let deadline = std::time::Instant::now() + hc.budget();
    loop {
        if runner::check_healthy_once(&hc.test, hc.timeout) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 后台模式 completed：持有 Child 句柄 try_wait 收割，退出码 0 即完成。
const COMPLETED_BUDGET_SECS: u64 = 60;

fn wait_completed_sync(child: &mut std::process::Child) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(COMPLETED_BUDGET_SECS);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return false,
        }
    }
}

/// 解析重启目标。显式名称必须存在于当前后台会话；空列表选择会话中的全部服务。
fn restart_targets(
    record: &session::Session,
    requested: &[String],
) -> Result<Vec<String>, String> {
    if requested.is_empty() {
        return Ok(record.services.iter().map(|s| s.name.clone()).collect());
    }

    let requested: HashSet<&str> = requested.iter().map(String::as_str).collect();
    for name in &requested {
        if !record.services.iter().any(|s| s.name == *name) {
            return Err(format!("service '{name}' is not in the background session"));
        }
    }
    Ok(record
        .services
        .iter()
        .filter(|s| requested.contains(s.name.as_str()))
        .map(|s| s.name.clone())
        .collect())
}

/// 停止一个后台服务，返回时原 pid 已退出；超时后升级为 SIGKILL。
fn stop_for_restart(service: &session::SessionService, stop_timeout: u64) -> Result<(), String> {
    if !session::is_alive(service.pid) {
        return Ok(());
    }

    runner::print_status(&format!("stopping {} (pid {})", service.name, service.pid));
    runner::signal_group(service.pid, graceful_signal());
    let exited = stop_timeout > 0
        && session::wait_exited(service.pid, Duration::from_secs(stop_timeout));
    if exited {
        return Ok(());
    }

    runner::signal_group(service.pid, sigkill());
    if session::wait_exited(service.pid, Duration::from_secs(2)) {
        Ok(())
    } else {
        Err(format!("service '{}' did not stop", service.name))
    }
}

/// 重启后台会话中的指定服务；已退出服务直接重新启动。
async fn restart_cmd(
    cfg: &Config,
    cfg_path: &Path,
    requested: Vec<String>,
    stop_timeout: u64,
) -> i32 {
    let Some(mut record) = session::load(cfg_path) else {
        eprintln!("error: no background session for {}", cfg_path.display());
        return 1;
    };
    let targets = match restart_targets(&record, &requested) {
        Ok(targets) if !targets.is_empty() => targets,
        Ok(_) => {
            eprintln!("error: background session has no services to restart");
            return 1;
        }
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    for name in &targets {
        if !cfg.services.contains_key(name) {
            eprintln!("error: service '{name}' is no longer defined in the configuration");
            return 1;
        }
    }

    let base = cfg_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut failures = 0;
    let mut restarted = 0;
    for name in targets {
        let index = record
            .services
            .iter()
            .position(|service| service.name == name)
            .expect("restart target belongs to session");
        let previous_pid = record.services[index].pid;
        if let Err(e) = stop_for_restart(&record.services[index], stop_timeout) {
            eprintln!("error: {e}");
            failures += 1;
            continue;
        }

        runner::print_status(&format!("starting {name}"));
        let service = &cfg.services[&name];
        let log = record.services[index].log.clone();
        match runner::spawn_detached(&name, service, &base, &log) {
            Ok(child) => {
                let pid = child.id();
                record.services[index].pid = pid;
                if let Err(e) = session::save(cfg_path, &record) {
                    runner::signal_group(pid, sigkill());
                    record.services[index].pid = previous_pid;
                    eprintln!("error: {e}");
                    return 1;
                }
                runner::print_status(&format!("restarted {name} (pid {pid})"));
                restarted += 1;
            }
            Err(e) => {
                eprintln!("error: {e}");
                failures += 1;
            }
        }
    }
    if failures > 0 {
        1
    } else {
        runner::print_status(&format!("restarted {restarted} service(s)"));
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
    if let Some(name) = services
        .iter()
        .find(|name| !record.services.iter().any(|service| service.name == **name))
    {
        eprintln!("error: service '{name}' is not in the background session");
        return 1;
    }

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

async fn up(
    cfg: &Config,
    cfg_path: &Path,
    selected: &BTreeSet<String>,
    stop_timeout: u64,
    no_color: bool,
) -> i32 {
    if selected.is_empty() {
        eprintln!("error: no services defined in {}", cfg_path.display());
        return 1;
    }

    let levels = match cfg.levels() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let required = cfg.required_conditions_for(selected);

    let colors_on = use_color(no_color);
    let names: Vec<&str> = selected.iter().map(String::as_str).collect();
    runner::print_status(&term::dim(&format!(
        "Using {} · {} service(s): {}",
        cfg_path.display(),
        names.len(),
        names.join(", ")
    )));

    let width = selected.iter().map(String::len).max().unwrap_or(0);
    let base = cfg_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let shutdown = Arc::new(Shutdown::new());
    let pids: SharedPids = Arc::new(Mutex::new(Vec::new()));
    let ready: Arc<Mutex<HashMap<String, bool>>> = Arc::new(Mutex::new(HashMap::new()));
    let (tx, mut rx) = mpsc::channel::<ServiceEvent>(64);
    let mut gates: HashMap<String, Arc<Notify>> = HashMap::new();

    for (i, (name, svc)) in cfg
        .services
        .iter()
        .filter(|(name, _)| selected.contains(*name))
        .enumerate()
    {
        let gate = Arc::new(Notify::new());
        gates.insert(name.clone(), gate.clone());
        let startup = Arc::new(runner::Startup {
            gate,
            ready: ready.clone(),
            deps: cfg.deps_of(name).keys().cloned().collect(),
            required: required.get(name).copied().unwrap_or(config::DepCondition::Started),
        });
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
            runner::run_service(name, svc, base, color, width, shutdown, tx, Some(startup)).await;
        });
    }
    drop(tx);

    // 第 0 波无依赖，立即触发。
    for name in levels[0].iter().filter(|name| selected.contains(*name)) {
        gates[name].notify_one();
    }

    let mut wave = 0usize;
    let mut settled: HashSet<String> = HashSet::new();
    let mut spawned: HashSet<String> = HashSet::new();
    let mut finished: HashSet<String> = HashSet::new();
    let mut any_failed = false;
    let mut shutting_down = false;
    loop {
        tokio::select! {
            ev = rx.recv() => {
                match ev {
                    Some(ServiceEvent::Started { name, pid }) => {
                        pids.lock().unwrap().push(pid);
                        spawned.insert(name);
                    }
                    Some(ServiceEvent::Ready { name }) => {
                        ready.lock().unwrap().insert(name.clone(), true);
                        settled.insert(name);
                    }
                    Some(ServiceEvent::StartFailed { name, error }) => {
                        runner::print_status(&term::paint("31", &error));
                        ready.lock().unwrap().insert(name.clone(), false);
                        settled.insert(name.clone());
                        finished.insert(name);
                        any_failed = true;
                    }
                    Some(ServiceEvent::Exited { name, code }) => {
                        finished.insert(name);
                        // 仅显式非零退出码记为失败；停机时被信号终止不计。
                        if matches!(code, Some(c) if c != 0) {
                            any_failed = true;
                        }
                    }
                    None => break,
                }
                // 当前波全部 settle → 触发下一波。
                while wave + 1 < levels.len()
                    && levels[wave]
                        .iter()
                        .filter(|name| selected.contains(*name))
                        .all(|n| settled.contains(n))
                {
                    wave += 1;
                    for name in levels[wave].iter().filter(|name| selected.contains(*name)) {
                        gates[name].notify_one();
                    }
                }
                // 结束判定：自然退出（全部最终事件）或停机后已启动服务全部结束。
                if !shutting_down && finished.len() == selected.len() {
                    break;
                }
                if shutting_down && spawned.iter().all(|n| finished.contains(n)) {
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

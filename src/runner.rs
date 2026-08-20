//! 服务进程编排：spawn、日志行流打印、重启策略与进程组信号。

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Child;
use tokio::sync::{mpsc, Notify};

use crate::config::{self, parse_env_file, Service};
use crate::term;

/// 重启前固定等待的秒数，避免崩溃热循环。
const RESTART_DELAY_SECS: u64 = 1;

/// 服务任务 → 主循环事件。
pub enum ServiceEvent {
    /// 进程已启动（每次 spawn 都会发，含重启）。
    Started { name: String, pid: u32 },
    /// 服务已就绪（依赖编排中本服务的依赖方可以启动了）。每个服务至多发一次。
    Ready { name: String },
    /// 启动失败（spawn 错误或就绪超时/退出非零）。依赖方将跳过。
    StartFailed { name: String, error: String },
    /// 进程最终退出（不再重启）。`code == None` 表示被信号终止。
    Exited { name: String, code: Option<i32> },
}

/// 启动编排上下文：闸门、依赖就绪表、依赖清单与最严就绪条件。
pub struct Startup {
    /// 闸门：main 触发后本服务才开始 spawn。
    pub gate: Arc<Notify>,
    /// 依赖就绪表（name → 就绪成功与否），main 维护。
    pub ready: Arc<Mutex<HashMap<String, bool>>>,
    /// 依赖服务名（启动前逐一检查）。
    pub deps: Vec<String>,
    /// 本服务作为依赖需满足的最严条件。
    pub required: config::DepCondition,
}

/// 停机信号：置位后唤醒所有等待者。`wait` 在信号到达前挂起。
pub struct Shutdown {
    flag: AtomicBool,
    notify: Notify,
}

impl Shutdown {
    pub fn new() -> Self {
        Shutdown { flag: AtomicBool::new(false), notify: Notify::new() }
    }

    pub fn trigger(&self) {
        self.flag.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// 挂起直到 `trigger`。若已触发则立即返回。
    pub async fn wait(&self) {
        let notified = self.notify.notified();
        if !self.is_set() {
            notified.await;
        }
    }
}

pub struct Line {
    pub text: String,
}

/// 字节级行读：按 `\n` 切行；EOF 时残留的字节（部分行）也作为一行发出，
/// 保证 `printf partial` 这类无换行输出在进程退出时不丢失。
async fn read_lines<R: AsyncRead + Unpin>(r: R, tx: mpsc::Sender<Line>) {
    let mut reader = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&buf).into_owned();
                if tx.send(Line { text }).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// 带服务名前缀的日志行输出（前台日志流与后台 logs 共用）。
pub fn print_line(name: &str, color: &str, width: usize, line: &Line) {
    use std::io::Write;
    let text = line.text.trim_end_matches(['\n', '\r']);
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}{}", term::prefix(name, width, color), text);
    let _ = out.flush();
}

/// 无前缀的状态消息（"exited with code"、停机提示等）。
pub fn print_status(s: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{s}");
    let _ = out.flush();
}

/// 启动方式：shell 命令或直接 exec；前台/后台共用。
pub(crate) enum LaunchSpec {
    Shell { command: String },
    Direct { program: String, args: Vec<String> },
}

pub(crate) fn launch_spec(svc: &Service) -> LaunchSpec {
    match &svc.program {
        Some(program) => LaunchSpec::Direct { program: program.clone(), args: svc.args.clone() },
        None => LaunchSpec::Shell { command: svc.command.clone().unwrap_or_default() },
    }
}

fn shell_name() -> &'static str {
    if cfg!(windows) {
        "cmd"
    } else {
        "sh"
    }
}

fn shell_flag() -> &'static str {
    if cfg!(windows) {
        "/C"
    } else {
        "-c"
    }
}

/// 解析并校验 cwd（相对配置目录），返回绝对路径。
pub(crate) fn resolve_cwd(
    name: &str,
    svc: &Service,
    base: &Path,
) -> Result<Option<PathBuf>, String> {
    let cwd = svc.cwd.as_ref().map(|c| {
        if c.is_absolute() {
            c.clone()
        } else {
            base.join(c)
        }
    });
    if let Some(cwd) = &cwd
        && !cwd.is_dir()
    {
        return Err(format!(
            "service '{name}': working directory '{}' does not exist",
            cwd.display()
        ));
    }
    Ok(cwd)
}

/// 读取 env_file 键值（相对配置目录）；env map 随后覆盖同名键。
fn env_file_entries(
    name: &str,
    svc: &Service,
    base: &Path,
) -> Result<Vec<(String, String)>, String> {
    let mut entries = Vec::new();
    if let Some(ef) = &svc.env_file {
        for f in ef.files() {
            let p = base.join(f);
            let content = std::fs::read_to_string(&p).map_err(|e| {
                format!("service '{name}': cannot read env_file {}: {e}", p.display())
            })?;
            entries.extend(parse_env_file(&content));
        }
    }
    Ok(entries)
}

/// 按配置 spawn 一个服务，并隔离其生命周期控制域：Unix 用进程组，Windows 用控制台进程组。
pub(crate) fn spawn_service(name: &str, svc: &Service, base: &Path) -> Result<Child, String> {
    let spec = launch_spec(svc);
    let mut cmd = match &spec {
        LaunchSpec::Shell { command } => {
            let mut c = tokio::process::Command::new(shell_name());
            c.arg(shell_flag()).arg(command);
            c
        }
        LaunchSpec::Direct { program, args } => {
            let mut c = tokio::process::Command::new(program);
            c.args(args);
            c
        }
    };

    let cwd = resolve_cwd(name, svc, base)?;
    if let Some(cwd) = &cwd {
        cmd.current_dir(cwd);
    }

    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env_file_entries(name, svc, base)? {
        cmd.env(k, v);
    }
    for (k, v) in &svc.env {
        cmd.env(k, v);
    }

    #[cfg(unix)]
    cmd.process_group(0);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
    }

    cmd.spawn().map_err(|e| format!("service '{name}': failed to start: {e}"))
}

/// 后台模式 spawn：stdout/stderr 直接重定向到日志文件（append），stdin null；
/// 工具退出后服务继续运行。返回 Child 句柄（持有它可收割退出状态，drop 不杀进程）。
pub fn spawn_detached(
    name: &str,
    svc: &Service,
    base: &Path,
    log_file: &Path,
) -> Result<std::process::Child, String> {
    let spec = launch_spec(svc);
    let mut cmd = match &spec {
        LaunchSpec::Shell { command } => {
            let mut c = std::process::Command::new(shell_name());
            c.arg(shell_flag()).arg(command);
            c
        }
        LaunchSpec::Direct { program, args } => {
            let mut c = std::process::Command::new(program);
            c.args(args);
            c
        }
    };

    let cwd = resolve_cwd(name, svc, base)?;
    if let Some(cwd) = &cwd {
        cmd.current_dir(cwd);
    }

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)
        .map_err(|e| format!("service '{name}': cannot open log {}: {e}", log_file.display()))?;
    let file_err = file
        .try_clone()
        .map_err(|e| format!("service '{name}': cannot clone log handle: {e}"))?;
    cmd.stdin(Stdio::null()).stdout(Stdio::from(file_err)).stderr(Stdio::from(file));
    for (k, v) in env_file_entries(name, svc, base)? {
        cmd.env(k, v);
    }
    for (k, v) in &svc.env {
        cmd.env(k, v);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
    }

    let child = cmd
        .spawn()
        .map_err(|e| format!("service '{name}': failed to start: {e}"))?;
    Ok(child)
}

/// 单个服务的完整生命周期：等闸门 → spawn → 就绪上报 → 日志流 → 退出 → 按策略重启。
/// 重启不重查依赖、不重报就绪（依赖编排只约束首轮启动）。
/// 参数均为任务上下文（名称/配置/展示/信号/事件/编排），聚合为结构体反而增加间接层。
#[allow(clippy::too_many_arguments)]
pub async fn run_service(
    name: String,
    svc: Service,
    base: PathBuf,
    color: String,
    width: usize,
    shutdown: Arc<Shutdown>,
    events: mpsc::Sender<ServiceEvent>,
    startup: Option<Arc<Startup>>,
) {
    let mut first = true;
    loop {
        // 首轮：等闸门 + 校验依赖就绪（重启轮次不再等待）。
        if let Some(s) = &startup
            && first
        {
            s.gate.notified().await;
            let failed_dep = {
                let ready_map = s.ready.lock().unwrap();
                s.deps
                    .iter()
                    .find(|dep| ready_map.get(*dep) != Some(&true))
                    .cloned()
            };
            if let Some(dep) = failed_dep {
                let msg =
                    format!("service '{name}' skipped: dependency '{dep}' failed to start");
                let _ = events
                    .send(ServiceEvent::StartFailed { name: name.clone(), error: msg })
                    .await;
                return;
            }
        }

        let mut child = match spawn_service(&name, &svc, &base) {
            Ok(c) => c,
            Err(e) => {
                let _ = events.send(ServiceEvent::StartFailed { name: name.clone(), error: e }).await;
                return;
            }
        };
        let pid = child.id().unwrap_or(0);
        let _ = events.send(ServiceEvent::Started { name: name.clone(), pid }).await;

        // 首轮就绪：started 条件立即满足。
        let mut settled = false;
        if let Some(s) = &startup
            && s.required == config::DepCondition::Started
        {
            let _ = events.send(ServiceEvent::Ready { name: name.clone() }).await;
            settled = true;
        }

        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let (line_tx, mut line_rx) = mpsc::channel::<Line>(256);
        let t_out = tokio::spawn(read_lines(stdout, line_tx.clone()));
        let t_err = tokio::spawn(read_lines(stderr, line_tx.clone()));
        drop(line_tx);

        // 健康轮询 future（仅首轮且要求 healthy）。
        let mut health: Option<Pin<Box<dyn Future<Output = bool> + Send>>> = match (&startup, first) {
            (Some(s), true) if s.required == config::DepCondition::Healthy => {
                let hc = svc.healthcheck.clone().expect("healthy required implies healthcheck");
                Some(Box::pin(wait_healthy(hc, &shutdown)))
            }
            _ => None,
        };

        // 边跑边打印；`child.wait()` 完成即退出 select。
        let mut wait = std::pin::pin!(child.wait());
        let status = loop {
            tokio::select! {
                res = &mut wait => break res,
                line = line_rx.recv() => {
                    if let Some(l) = line {
                        print_line(&name, &color, width, &l);
                    }
                }
                healthy = async { health.as_mut().expect("health future").await },
                    if health.is_some() =>
                {
                    if healthy {
                        let _ = events.send(ServiceEvent::Ready { name: name.clone() }).await;
                    } else {
                        let msg = format!("service '{name}' failed health check within budget");
                        let _ = events
                            .send(ServiceEvent::StartFailed { name: name.clone(), error: msg })
                            .await;
                    }
                    settled = true;
                    health = None;
                }
            }
        };
        // 冲刷 reader 可能残留的部分行，再决定是否重启。
        while let Some(l) = line_rx.recv().await {
            print_line(&name, &color, width, &l);
        }
        let _ = t_out.await;
        let _ = t_err.await;

        let code = status.ok().and_then(|s| s.code());

        // 首轮收尾就绪：healthy/completed 条件按退出码判定。
        if !settled
            && first
            && let Some(s) = &startup
        {
            match s.required {
                config::DepCondition::Healthy | config::DepCondition::Completed => {
                    if code == Some(0) {
                        let _ = events.send(ServiceEvent::Ready { name: name.clone() }).await;
                    } else {
                        let msg = format!(
                            "service '{name}' exited (code {}) before becoming ready",
                            code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into())
                        );
                        let _ = events
                            .send(ServiceEvent::StartFailed { name: name.clone(), error: msg })
                            .await;
                    }
                }
                config::DepCondition::Started => {}
            }
        }

        let exit_msg = match code {
            Some(c) => format!("{name} exited with code {c}"),
            None => format!("{name} terminated by signal"),
        };
        print_status(&term::dim(&exit_msg));

        if shutdown.is_set() || !svc.restart.should_restart(code == Some(0)) {
            let _ = events.send(ServiceEvent::Exited { name: name.clone(), code }).await;
            return;
        }

        print_status(&term::dim(&format!("{name} restarting...")));
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(RESTART_DELAY_SECS)) => {}
            _ = shutdown.wait() => {}
        }
        if shutdown.is_set() {
            let _ = events.send(ServiceEvent::Exited { name: name.clone(), code }).await;
            return;
        }
        first = false;
    }
}

/// 单次健康检查：运行 `test`，退出 0 = 健康；超时按失败计。
pub fn check_healthy_once(test: &str, timeout: Duration) -> bool {
    let Ok(mut child) = std::process::Command::new(shell_name())
        .arg(shell_flag())
        .arg(test)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return false,
        }
    }
}

/// 预算内轮询健康检查：成功一次即 true；预算耗尽或停机返回 false。
pub async fn wait_healthy(hc: config::Healthcheck, shutdown: &Shutdown) -> bool {
    let deadline = tokio::time::Instant::now() + hc.budget();
    let mut interval = tokio::time::interval(hc.interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if shutdown.is_set() {
            return false;
        }
        if check_healthy_once(&hc.test, hc.timeout) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown.wait() => return false,
        }
    }
}

/// 请求服务及其子进程优雅退出。Unix 发 `SIGTERM`；Windows 发 `CTRL_BREAK_EVENT`。
#[cfg(unix)]
pub fn request_stop(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
}

/// 请求服务及其子进程优雅退出。先尝试当前控制台；失败时附着目标控制台重试。
#[cfg(windows)]
pub fn request_stop(pid: u32) {
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, CTRL_BREAK_EVENT, FreeConsole, GenerateConsoleCtrlEvent,
    };

    unsafe {
        if GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) != 0 {
            return;
        }
        let _ = FreeConsole();
        if AttachConsole(pid) != 0 {
            let _ = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
            let _ = FreeConsole();
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

/// 强制终止服务及其子进程。Unix 终止进程组；Windows 使用系统树终止命令。
#[cfg(unix)]
pub fn force_stop(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(windows)]
pub fn force_stop(pid: u32) {
    let _ = std::process::Command::new("taskkill.exe")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn request_stop_all(pids: &[u32]) {
    for &pid in pids {
        request_stop(pid);
    }
}

pub fn force_stop_all(pids: &[u32]) {
    for &pid in pids {
        force_stop(pid);
    }
}

/// 主循环持有的共享 pid 表：Started 事件追加，停机/超时任务按快照发送信号。
pub type SharedPids = Arc<Mutex<Vec<u32>>>;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn read_lines_flushes_partial_at_eof() {
        let (mut w, r) = tokio::io::duplex(64);
        tokio::spawn(async move {
            w.write_all(b"hello").await.unwrap();
            drop(w);
        });
        let (tx, mut rx) = mpsc::channel(8);
        read_lines(r, tx).await;
        let mut out = Vec::new();
        while let Some(l) = rx.recv().await {
            out.push(l.text);
        }
        assert_eq!(out, vec!["hello"]);
    }

    #[tokio::test]
    async fn read_lines_splits_on_newline() {
        let (mut w, r) = tokio::io::duplex(64);
        tokio::spawn(async move {
            w.write_all(b"a\nb\nc").await.unwrap();
            drop(w);
        });
        let (tx, mut rx) = mpsc::channel(8);
        read_lines(r, tx).await;
        let mut out = Vec::new();
        while let Some(l) = rx.recv().await {
            out.push(l.text);
        }
        assert_eq!(out, vec!["a\n", "b\n", "c"]);
    }

    #[tokio::test]
    async fn shutdown_wait_returns_when_already_triggered() {
        let s = Arc::new(Shutdown::new());
        s.trigger();
        tokio::time::timeout(Duration::from_secs(1), s.wait()).await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_wait_unblocks_on_trigger() {
        let s = Arc::new(Shutdown::new());
        let s2 = s.clone();
        let t = tokio::spawn(async move { s2.wait().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        s.trigger();
        tokio::time::timeout(Duration::from_secs(1), t).await.unwrap().unwrap();
    }
}

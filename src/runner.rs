//! 服务进程编排：spawn、日志行流打印、重启策略与进程组信号。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Notify};

use crate::config::Service;
use crate::term;

/// 重启前固定等待的秒数，避免崩溃热循环。
const RESTART_DELAY_SECS: u64 = 1;

/// 服务任务 → 主循环事件。
pub enum ServiceEvent {
    /// 进程已启动。
    Started { pid: u32 },
    /// 进程最终退出（不再重启）。`code == None` 表示被信号终止。
    Exited { code: Option<i32> },
    /// spawn 失败（视为最终失败，不触发重启策略）。
    Failed,
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

/// 带服务名前缀的日志行输出。
fn print_line(name: &str, color: &str, width: usize, line: &Line) {
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

/// 按配置 spawn 一个服务；unix 下子进程成为新进程组组长（`process_group(0)`），
/// 便于对整棵进程树发信号。
pub(crate) fn spawn_service(name: &str, svc: &Service, base: &Path) -> Result<Child, String> {
    let mut cmd = match &svc.program {
        Some(program) => {
            let mut c = Command::new(program);
            c.args(&svc.args);
            c
        }
        None => {
            let (shell, flag) = if cfg!(windows) { ("cmd", "/C") } else { ("sh", "-c") };
            let mut c = Command::new(shell);
            c.arg(flag).arg(svc.command.as_deref().unwrap_or_default());
            c
        }
    };

    let cwd = svc.cwd.as_ref().map(|c| {
        if c.is_absolute() {
            c.clone()
        } else {
            base.join(c)
        }
    });
    if let Some(cwd) = &cwd {
        if !cwd.is_dir() {
            return Err(format!(
                "service '{name}': working directory '{}' does not exist",
                cwd.display()
            ));
        }
        cmd.current_dir(cwd);
    }

    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in &svc.env {
        cmd.env(k, v);
    }

    #[cfg(unix)]
    cmd.process_group(0);

    cmd.spawn().map_err(|e| format!("service '{name}': failed to start: {e}"))
}

/// 单个服务的完整生命周期：spawn → 日志流 → 退出 → 按策略重启，直到停机或不再重启。
pub async fn run_service(
    name: String,
    svc: Service,
    base: PathBuf,
    color: String,
    width: usize,
    shutdown: Arc<Shutdown>,
    events: mpsc::Sender<ServiceEvent>,
) {
    loop {
        let mut child = match spawn_service(&name, &svc, &base) {
            Ok(c) => c,
            Err(e) => {
                print_status(&term::paint("31", &e));
                let _ = events.send(ServiceEvent::Failed).await;
                return;
            }
        };
        let pid = child.id().unwrap_or(0);
        let _ = events.send(ServiceEvent::Started { pid }).await;

        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let (line_tx, mut line_rx) = mpsc::channel::<Line>(256);
        let t_out = tokio::spawn(read_lines(stdout, line_tx.clone()));
        let t_err = tokio::spawn(read_lines(stderr, line_tx.clone()));
        drop(line_tx);

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
            }
        };
        // 冲刷 reader 可能残留的部分行，再决定是否重启。
        while let Some(l) = line_rx.recv().await {
            print_line(&name, &color, width, &l);
        }
        let _ = t_out.await;
        let _ = t_err.await;

        let code = status.ok().and_then(|s| s.code());
        let exit_msg = match code {
            Some(c) => format!("{name} exited with code {c}"),
            None => format!("{name} terminated by signal"),
        };
        print_status(&term::dim(&exit_msg));

        if shutdown.is_set() || !svc.restart.should_restart(code == Some(0)) {
            let _ = events.send(ServiceEvent::Exited { code }).await;
            return;
        }

        print_status(&term::dim(&format!("{name} restarting...")));
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(RESTART_DELAY_SECS)) => {}
            _ = shutdown.wait() => {}
        }
        if shutdown.is_set() {
            let _ = events.send(ServiceEvent::Exited { code }).await;
            return;
        }
    }
}

/// 向进程组发信号。unix 下 `kill(-pid, sig)` 覆盖整棵进程树；
/// 其他平台退化为 taskkill 整树终止。
#[cfg(unix)]
pub fn signal_group(pid: u32, sig: i32) {
    // 组已不存在（ESRCH）等情况静默忽略。
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}

#[cfg(not(unix))]
pub fn signal_group(pid: u32, _sig: i32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .status();
}

pub fn signal_all(pids: &[u32], sig: i32) {
    for &pid in pids {
        signal_group(pid, sig);
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

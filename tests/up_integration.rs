//! 端到端集成测试：真实二进制 + fixture 项目，验证 up/config/validate 全流程。
#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_yun-dev-manage");

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name)
}

fn run_bin(cwd: &std::path::Path, args: &[&str]) -> std::process::Child {
    Command::new(BIN)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn binary")
}

fn kill_int(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGINT);
    }
}

#[test]
fn up_streams_logs_and_graceful_shutdown_on_sigint() {
    let child = run_bin(&fixture("demo"), &["up"]);
    // frontend 打印 3 行后 ~0.7s 退出，worker 立即 exit 3，backend 持续运行；
    // 1.6s 后 SIGINT，工具应优雅停止全部服务。
    std::thread::sleep(Duration::from_millis(1600));
    kill_int(child.id());
    let out = child.wait_with_output().expect("wait for exit");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(out.status.code(), Some(1), "worker failed -> tool must exit 1:\n{stdout}");
    assert!(stdout.contains("frontend  | frontend log 1"), "prefixed line missing:\n{stdout}");
    assert!(stdout.contains("frontend  | frontend log 3"), "prefixed line missing:\n{stdout}");
    assert!(stdout.contains("partial-tail"), "partial (no-newline) line not flushed:\n{stdout}");
    assert!(stdout.contains("worker    | worker booting"), "worker line missing:\n{stdout}");
    assert!(stdout.contains("worker exited with code 3"), "worker exit message missing:\n{stdout}");
    assert!(stdout.contains("frontend exited with code 0"), "frontend exit message missing:\n{stdout}");
    assert!(stdout.contains("Gracefully stopping..."), "graceful message missing:\n{stdout}");
    assert!(
        stdout.contains("backend terminated by signal")
            || stdout.contains("backend exited with code"),
        "backend exit message missing:\n{stdout}"
    );
}

#[test]
fn on_failure_restarts_service() {
    let child = run_bin(&fixture("restart"), &["up"]);
    // flaky 每 ~1s 退出一次并重启；2.5s 内应至少看到 2 次退出 + 重启提示。
    std::thread::sleep(Duration::from_millis(2500));
    kill_int(child.id());
    let out = child.wait_with_output().expect("wait for exit");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(out.status.code(), Some(1), "flaky exited non-zero -> tool exits 1:\n{stdout}");
    let exits = stdout.matches("flaky exited with code 1").count();
    assert!(exits >= 2, "expected >=2 exits, got {exits}:\n{stdout}");
    assert!(stdout.contains("flaky restarting..."), "restart hint missing:\n{stdout}");
}

#[test]
fn stop_timeout_zero_forces_immediate_kill() {
    let child = run_bin(&fixture("demo"), &["up", "--stop-timeout", "0"]);
    std::thread::sleep(Duration::from_millis(1600));
    kill_int(child.id());
    let out = child.wait_with_output().expect("wait for exit");
    assert_eq!(out.status.code(), Some(1), "worker exited 3 -> exit 1");
    assert!(String::from_utf8_lossy(&out.stdout).contains("Gracefully stopping..."));
}

#[test]
fn validate_and_config_subcommands() {
    let v = Command::new(BIN)
        .current_dir(fixture("demo"))
        .arg("validate")
        .output()
        .expect("run validate");
    assert!(v.status.success(), "validate must pass on demo fixture");
    assert!(String::from_utf8_lossy(&v.stdout).contains("OK:"));

    let c = Command::new(BIN)
        .current_dir(fixture("demo"))
        .arg("config")
        .output()
        .expect("run config");
    assert!(c.status.success(), "config must pass on demo fixture");
    let s = String::from_utf8_lossy(&c.stdout);
    assert!(s.contains("\"frontend\"") && s.contains("\"worker\""), "{s}");
}

#[test]
fn errors_when_config_missing_or_invalid() {
    let empty = std::env::temp_dir().join(format!("ydev-empty-{}", std::process::id()));
    std::fs::create_dir_all(&empty).unwrap();
    let e = Command::new(BIN).current_dir(&empty).arg("up").output().expect("run up");
    assert_eq!(e.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&e.stderr).contains(".yun-dev.json"));
    std::fs::remove_dir_all(&empty).ok();

    let dir = std::env::temp_dir().join(format!("ydev-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("bad.json"), r#"{"services":{"x":{}}}"#).unwrap();
    let out = Command::new(BIN)
        .current_dir(&dir)
        .args(["validate", "--file", "bad.json"])
        .output()
        .expect("run validate");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs 'command' or 'program'"));
    std::fs::remove_dir_all(&dir).ok();
}

//! 依赖编排端到端测试：启动顺序、健康检查、completed 条件、失败传播。
#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_yun-dev-manage");

static ORDER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name)
}

fn run(cwd: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(BIN)
        .current_dir(cwd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run binary");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// 前台 up + SIGINT 优雅停机，返回完整输出。
fn up_then_sigint(cwd: &std::path::Path, wait_ms: u64) -> (i32, String) {
    let child = Command::new(BIN)
        .current_dir(cwd)
        .arg("up")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn up");
    std::thread::sleep(Duration::from_millis(wait_ms));
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let out = child.wait_with_output().expect("wait");
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn dependency_graph_starts_all_services() {
    let _lock = ORDER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let order = fixture("order");
    let _ = std::fs::remove_file("/tmp/ydev-order-db-ready");
    let (code, out) = up_then_sigint(&order, 2000);
    assert_eq!(code, 0, "all services exited 0 before SIGINT:\n{out}");
    assert!(out.contains("db up"), "db must start:\n{out}");
    assert!(out.contains("backend started"), "backend must start:\n{out}");
    assert!(out.contains("frontend started"), "frontend must start:\n{out}");
    assert!(out.contains("migrated") && out.contains("app up"), "all dependency paths must run:\n{out}");
    let _ = std::fs::remove_file("/tmp/ydev-order-db-ready");
}

#[test]
fn unhealthy_dependency_blocks_dependents() {
    let _lock = ORDER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let faildep = fixture("faildep");
    // db 健康检查 200ms × 3 预算内永不通过 → backend 跳过 → 退出码 1。
    let (code, out, _) = run(&faildep, &["up"]);
    assert_eq!(code, 1, "unhealthy dependency must fail the session:\n{out}");
    assert!(out.contains("db failed"), "{out}");
    assert!(out.contains("skipped: dependency 'db'"), "{out}");
    assert!(!out.contains("backend up"), "backend must not start:\n{out}");
}

#[test]
fn up_detached_waits_for_dependencies() {
    let order = fixture("order");
    let _lock = ORDER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ = std::fs::remove_file("/tmp/ydev-order-db-ready");
    let (code, out, err) = run(&order, &["up", "-d"]);
    assert_eq!(code, 0, "up -d must exit 0: {out} {err}");
    assert!(out.contains("started 5 service(s)"), "{out}");

    // 就绪后才返回：ps 应显示全部服务（migrate 可能已 exited）。
    let (code, out, _) = run(&order, &["ps"]);
    assert_eq!(code, 0, "{out}");
    for name in ["migrate", "db", "backend", "app", "frontend"] {
        assert!(out.contains(name), "ps must list {name}:\n{out}");
    }

    let (code, _, _) = run(&order, &["down"]);
    assert_eq!(code, 0);
    let _ = std::fs::remove_file("/tmp/ydev-order-db-ready");
}

//! 后台模式端到端测试：up -d / ps / logs（含 -f 与 --tail）/ down / 互斥。
#![cfg(unix)]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_yun-dev-manage");

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name)
}

fn run_bin(cwd: &std::path::Path, args: &[&str]) -> Child {
    Command::new(BIN)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn binary")
}

fn run_and_capture(cwd: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(BIN)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run binary");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// 把子进程 stdout 读到字符串（直到 EOF），返回句柄便于之后 join。
fn collect_stdout(child: Child) -> (JoinHandle<String>, Child) {
    let mut child = child;
    let mut stdout = child.stdout.take().expect("stdout piped");
    let handle = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    (handle, child)
}

#[test]
fn up_detached_ps_logs_down_roundtrip() {
    let demo = fixture("demo");

    // up -d：全部 spawn 成功 → 0
    let (code, out, _) = run_and_capture(&demo, &["up", "-d"]);
    assert_eq!(code, 0, "up -d must exit 0: {out}");
    assert!(out.contains("started 3 service(s)"), "{out}");

    // ps：全部 running
    let (code, out, _) = run_and_capture(&demo, &["ps"]);
    assert_eq!(code, 0, "ps must exit 0: {out}");
    assert!(out.contains("frontend") && out.contains("running"), "{out}");
    assert!(out.contains("backend") && out.contains("running"), "{out}");

    // 等 frontend（~0.7s 后退出）与 worker（立即 exit 3）结束
    std::thread::sleep(Duration::from_millis(1500));
    let (_, out, _) = run_and_capture(&demo, &["ps"]);
    assert!(out.contains("frontend") && out.contains("exited"), "{out}");
    assert!(out.contains("backend") && out.contains("running"), "{out}");

    // logs：带前缀的历史日志
    let (code, out, _) = run_and_capture(&demo, &["logs"]);
    assert_eq!(code, 0);
    assert!(out.contains("frontend  | frontend log 1"), "{out}");
    assert!(out.contains("frontend  | partial-tail"), "{out}");
    assert!(out.contains("worker    | worker booting"), "{out}");

    // logs --tail N 只显示最后 N 行（无换行结尾的 partial 行也算一行）
    let (_, out, _) = run_and_capture(&demo, &["logs", "--tail", "1", "frontend"]);
    let lines: Vec<&str> = out.trim().lines().collect();
    assert_eq!(lines.len(), 1, "tail 1 must show one line: {out}");
    assert!(lines[0].contains("partial-tail"), "tail 1 = last line:\n{out}");
    let (_, out, _) = run_and_capture(&demo, &["logs", "--tail", "2", "frontend"]);
    let lines: Vec<&str> = out.trim().lines().collect();
    assert_eq!(lines.len(), 2, "tail 2 must show two lines: {out}");
    assert!(lines[0].contains("frontend log 3"), "{out}");
    assert!(lines[1].contains("partial-tail"), "{out}");

    // logs 服务过滤
    let (_, out, _) = run_and_capture(&demo, &["logs", "worker"]);
    assert!(out.contains("worker"), "{out}");
    assert!(!out.contains("frontend"), "filtered out: {out}");

    // 互斥：已有运行中后台会话时，up -d 与前台 up 都拒绝
    let (code, _, err) = run_and_capture(&demo, &["up", "-d"]);
    assert_eq!(code, 1);
    assert!(err.contains("background session is running"), "{err}");
    let (code, _, err) = run_and_capture(&demo, &["up"]);
    assert_eq!(code, 1);
    assert!(err.contains("background session is running"), "{err}");

    // down：仅 backend 仍在运行
    let (code, out, _) = run_and_capture(&demo, &["down"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("stopped 1 service(s)"), "{out}");

    // 会话已清除
    let (code, out, _) = run_and_capture(&demo, &["ps"]);
    assert_eq!(code, 1);
    assert!(out.contains("no background session"), "{out}");
    let (code, _, _) = run_and_capture(&demo, &["down"]);
    assert_eq!(code, 1);
}

#[test]
fn logs_follow_captures_new_lines() {
    let logging = fixture("logging");

    let (code, out, _) = run_and_capture(&logging, &["up", "-d"]);
    assert_eq!(code, 0, "up -d must exit 0: {out}");

    // 立即起 logs --follow：应能捕获后续新写入的 tick 行
    let child = run_bin(&logging, &["logs", "--follow"]);
    let (reader, mut child) = collect_stdout(child);
    std::thread::sleep(Duration::from_millis(2200));
    child.kill().expect("kill logs");
    child.wait().expect("reap logs");
    let collected = reader.join().expect("join reader");

    assert!(
        collected.contains("ticker  | tick 5"),
        "follow must capture lines written after start:\n{collected}"
    );

    // 清理会话（ticker 仍在 sleep）
    let (code, out, _) = run_and_capture(&logging, &["down"]);
    assert_eq!(code, 0, "{out}");
}

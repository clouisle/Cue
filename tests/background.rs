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

fn pid_for(ps: &str, service: &str) -> u32 {
    ps.lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some(service)).then(|| fields.next()?.parse::<u32>().ok())?
        })
        .unwrap_or_else(|| panic!("missing pid for {service}:\n{ps}"))
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
fn targeted_up_and_restart_manage_only_requested_services() {
    let targeted = fixture("targeted");

    // `up -d api` includes api 的依赖 database，但不启动无关 worker。
    let (code, out, err) = run_and_capture(&targeted, &["up", "-d", "api"]);
    assert_eq!(code, 0, "up -d api must exit 0: {out} {err}");
    assert!(out.contains("started 2 service(s)"), "{out}");
    let (_, ps, _) = run_and_capture(&targeted, &["ps"]);
    assert!(ps.contains("database") && ps.contains("api"), "{ps}");
    assert!(!ps.contains("worker"), "unrelated service started:\n{ps}");
    let database_pid = pid_for(&ps, "database");
    let api_pid = pid_for(&ps, "api");

    // 指定 restart 只替换 api 的 pid；其依赖保持运行。
    let (code, out, err) = run_and_capture(&targeted, &["restart", "api"]);
    assert_eq!(code, 0, "restart api must exit 0: {out} {err}");
    let (_, ps, _) = run_and_capture(&targeted, &["ps"]);
    assert_eq!(pid_for(&ps, "database"), database_pid, "dependency was restarted:\n{ps}");
    assert_ne!(pid_for(&ps, "api"), api_pid, "api pid was not replaced:\n{ps}");
    let (_, logs, _) = run_and_capture(&targeted, &["logs", "api"]);
    assert!(logs.matches("api started").count() >= 2, "restart did not append a second start:\n{logs}");

    let (code, _, err) = run_and_capture(&targeted, &["logs", "missing"]);
    assert_eq!(code, 1);
    assert!(err.contains("not in the background session"), "{err}");

    // 不存在的 session 服务不得影响已运行的 api。
    let api_pid = pid_for(&ps, "api");
    let (code, _, err) = run_and_capture(&targeted, &["restart", "missing"]);
    assert_eq!(code, 1);
    assert!(err.contains("not in the background session"), "{err}");
    let (_, ps, _) = run_and_capture(&targeted, &["ps"]);
    assert_eq!(pid_for(&ps, "api"), api_pid, "unknown restart changed api:\n{ps}");

    let (code, out, _) = run_and_capture(&targeted, &["down"]);
    assert_eq!(code, 0, "{out}");

    // 无名称 restart 覆盖当前后台会话中的全部服务。
    let (code, out, err) = run_and_capture(&targeted, &["up", "-d"]);
    assert_eq!(code, 0, "up -d must exit 0: {out} {err}");
    let (_, ps, _) = run_and_capture(&targeted, &["ps"]);
    let database_pid = pid_for(&ps, "database");
    let api_pid = pid_for(&ps, "api");
    let worker_pid = pid_for(&ps, "worker");
    let (code, out, err) = run_and_capture(&targeted, &["restart"]);
    assert_eq!(code, 0, "restart all must exit 0: {out} {err}");
    let (_, ps, _) = run_and_capture(&targeted, &["ps"]);
    assert_ne!(pid_for(&ps, "database"), database_pid, "database was not restarted:\n{ps}");
    assert_ne!(pid_for(&ps, "api"), api_pid, "api was not restarted:\n{ps}");
    assert_ne!(pid_for(&ps, "worker"), worker_pid, "worker was not restarted:\n{ps}");
    let (code, out, _) = run_and_capture(&targeted, &["down"]);
    assert_eq!(code, 0, "{out}");

    // 前台 `up SERVICE` 复用同一依赖闭包，也不会启动 worker。
    let child = run_bin(&targeted, &["up", "api"]);
    std::thread::sleep(Duration::from_millis(400));
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let out = child.wait_with_output().expect("wait foreground up");
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("database started") && stdout.contains("api started"), "{stdout}");
    assert!(!stdout.contains("worker started"), "unrelated service started:\n{stdout}");
}

#[test]
fn logs_follow_captures_new_lines() {
    let logging = fixture("logging");

    let (code, out, _) = run_and_capture(&logging, &["up", "-d"]);
    assert_eq!(code, 0, "up -d must exit 0: {out}");

    // 立即起 logs -f ticker：应能捕获后续新写入的 tick 行
    let child = run_bin(&logging, &["logs", "-f", "ticker"]);
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

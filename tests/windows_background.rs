//! Windows 后台生命周期端到端测试：状态、日志、restart、树终止。
#![cfg(windows)]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BIN: &str = env!("CARGO_BIN_EXE_cue");

fn project_dir() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("cue-windows-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    std::fs::write(
        dir.join(".cue.json"),
        r#"{
          "services": {
            "ticker": {
              "command": "echo tick-1 & ping -n 3 127.0.0.1 >nul & echo tick-2 & ping -n 30 127.0.0.1 >nul"
            }
          }
        }"#,
    )
    .expect("write fixture config");
    dir
}

fn run(cwd: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(BIN)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run cue");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A command that creates detached services can leak its output handle into the service tree on Windows.
/// Run it without pipes so the test observes the CLI exit, not the service's eventual EOF.
fn run_lifecycle(cwd: &std::path::Path, args: &[&str]) -> i32 {
    Command::new(BIN)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run lifecycle command")
        .code()
        .unwrap_or(-1)
}

fn run_bin(cwd: &std::path::Path, args: &[&str]) -> Child {
    Command::new(BIN)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cue")
}

fn collect_stdout(child: Child) -> (JoinHandle<String>, Child) {
    let mut child = child;
    let mut stdout = child.stdout.take().expect("stdout piped");
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        stdout.read_to_string(&mut text).expect("read logs stdout");
        text
    });
    (reader, child)
}

fn pid_for(ps: &str, service: &str) -> u32 {
    ps.lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some(service)).then(|| fields.next()?.parse::<u32>().ok())?
        })
        .unwrap_or_else(|| panic!("missing pid for {service}:\n{ps}"))
}

#[test]
fn foreground_command_streams_logs_on_windows() {
    let project = project_dir();
    std::fs::write(
        project.join(".cue.json"),
        r#"{
          "services": {
            "one-shot": { "command": "echo foreground-ready & ping -n 2 127.0.0.1 >nul" }
          }
        }"#,
    )
    .expect("write foreground config");

    let (code, out, err) = run(&project, &["up"]);
    assert_eq!(code, 0, "foreground up failed:\n{out}\n{err}");
    assert!(out.contains("one-shot  | foreground-ready"), "{out}");
    std::fs::remove_dir_all(project).ok();
}

#[test]
fn detached_services_report_restart_follow_and_stop_on_windows() {
    let project = project_dir();

    let code = run_lifecycle(&project, &["up", "-d"]);
    assert_eq!(code, 0, "up -d failed");

    let (_, ps, _) = run(&project, &["ps"]);
    assert!(ps.contains("ticker") && ps.contains("running"), "{ps}");
    let first_pid = pid_for(&ps, "ticker");

    let (_, history, _) = run(&project, &["logs", "ticker"]);
    assert!(history.contains("ticker  | tick-1"), "{history}");

    let child = run_bin(&project, &["logs", "-f", "ticker"]);
    let (reader, mut child) = collect_stdout(child);
    std::thread::sleep(Duration::from_secs(3));
    child.kill().expect("stop follow command");
    child.wait().expect("reap follow command");
    let followed = reader.join().expect("join log reader");
    assert!(followed.contains("ticker  | tick-2"), "{followed}");

    let code = run_lifecycle(&project, &["--stop-timeout", "0", "restart", "ticker"]);
    assert_eq!(code, 0, "restart failed");
    let (_, ps, _) = run(&project, &["ps"]);
    assert!(ps.contains("ticker") && ps.contains("running"), "{ps}");
    assert_ne!(pid_for(&ps, "ticker"), first_pid, "restart retained pid:\n{ps}");

    let code = run_lifecycle(&project, &["--stop-timeout", "0", "down"]);
    assert_eq!(code, 0, "down failed");
    let (code, out, _) = run(&project, &["ps"]);
    assert_eq!(code, 1, "session was not cleared:\n{out}");

    std::fs::remove_dir_all(project).ok();
}

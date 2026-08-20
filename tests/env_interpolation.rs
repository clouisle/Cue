//! 变量插值端到端测试：.env 插值、env_file 注入、env 覆盖优先级。
#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_cue");

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name)
}

#[test]
fn interpolation_and_env_injection_end_to_end() {
    let envdemo = fixture("envdemo");
    let child = Command::new(BIN)
        .current_dir(&envdemo)
        .arg("up")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn up");
    std::thread::sleep(Duration::from_millis(1200));
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let out = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "clean SIGINT stop:\n{stdout}");

    // 加载期插值：.env 提供 PORT；svc.env 不参与插值 → TOKEN 走默认值。
    assert!(
        stdout.contains("interp port=8080 token=nope"),
        ".env interpolation + default missing:\n{stdout}"
    );
    // 运行期注入：env_file 注入 FROM_FILE，env map 覆盖 TOKEN。
    assert!(
        stdout.contains("injected token=overridden file=yes map=yes"),
        "env_file injection + env override:\n{stdout}"
    );
}

#[test]
fn up_detached_injects_env_file() {
    let envdemo = fixture("envdemo");
    let out = Command::new(BIN)
        .current_dir(&envdemo)
        .args(["up", "-d"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("up -d");
    assert_eq!(out.status.code().unwrap_or(-1), 0);

    // 等日志落盘后检查注入值。
    std::thread::sleep(Duration::from_millis(1000));
    let out = Command::new(BIN)
        .current_dir(&envdemo)
        .args(["logs", "web"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("logs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("interp port=8080 token=nope"), "{stdout}");
    assert!(stdout.contains("injected token=overridden file=yes map=yes"), "{stdout}");

    let out = Command::new(BIN)
        .current_dir(&envdemo)
        .arg("down")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("down");
    assert_eq!(out.status.code().unwrap_or(-1), 0);
}

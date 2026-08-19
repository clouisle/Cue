//! 后台会话状态：cache 目录定位、state.json 读写、pid 存活检测。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const STATE_VERSION: u32 = 1;
const STATE_FILE: &str = "state.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionService {
    pub name: String,
    pub pid: u32,
    /// 该服务的日志文件（append，原始输出，无前缀）。
    pub log: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub config_path: PathBuf,
    pub services: Vec<SessionService>,
}

/// 会话目录：`$XDG_CACHE_HOME/yun-dev-manage/<配置路径hash>`，
/// fallback `~/.cache/yun-dev-manage/<hash>`（macOS 同）。
pub fn session_dir(config_path: &Path) -> PathBuf {
    let mut h = DefaultHasher::new();
    let canon = config_path.canonicalize().unwrap_or_else(|_| config_path.to_path_buf());
    canon.hash(&mut h);
    cache_root().join(format!("{:016x}", h.finish()))
}

fn cache_root() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(x).join("yun-dev-manage");
    }
    #[cfg(unix)]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".cache").join("yun-dev-manage");
    }
    std::env::temp_dir().join("yun-dev-manage")
}

pub fn state_path(config_path: &Path) -> PathBuf {
    session_dir(config_path).join(STATE_FILE)
}

/// 读取状态文件；不存在或损坏时返回 `None`（损坏时顺带清理）。
pub fn load(config_path: &Path) -> Option<Session> {
    let p = state_path(config_path);
    let raw = std::fs::read_to_string(&p).ok()?;
    match serde_json::from_str::<Session>(&raw) {
        Ok(s) if s.version == STATE_VERSION => Some(s),
        _ => {
            std::fs::remove_file(&p).ok();
            None
        }
    }
}

/// 写状态文件；失败返回错误消息。
pub fn save(config_path: &Path, session: &Session) -> Result<(), String> {
    let dir = session_dir(config_path);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create session dir {}: {e}", dir.display()))?;
    let raw = serde_json::to_string_pretty(session).expect("session serializes");
    std::fs::write(state_path(config_path), raw)
        .map_err(|e| format!("cannot write session state: {e}"))
}

/// 删除状态文件（日志文件保留）。
pub fn clear(config_path: &Path) {
    std::fs::remove_file(state_path(config_path)).ok();
}

/// pid 存活检测：`kill(pid, 0)` 成功或 EPERM 视为存活，ESRCH 视为已退出。
#[cfg(unix)]
pub fn is_alive(pid: u32) -> bool {
    unsafe {
        libc::kill(pid as i32, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(not(unix))]
pub fn is_alive(pid: u32) -> bool {
    let _ = pid;
    false
}

impl Session {
    /// 是否有至少一个服务存活。
    pub fn is_any_running(&self) -> bool {
        self.services.iter().any(|s| is_alive(s.pid))
    }

    /// 全部服务在 `timeout` 内退出则返回 true（轮询 50ms）。
    pub fn wait_all_exited(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if !self.is_any_running() {
                return true;
            }
            if Instant::now() >= deadline {
                return !self.is_any_running();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_dir_is_stable_and_scoped() {
        let a = Path::new("/tmp/proj/.yun-dev.json");
        let b = Path::new("/tmp/other/.yun-dev.json");
        let d1 = session_dir(a);
        let d2 = session_dir(a);
        assert_eq!(d1, d2);
        assert_ne!(d1, session_dir(b));
    }

    #[test]
    fn save_load_roundtrip() {
        let cfg = Path::new("/tmp/proj/.yun-dev.json");
        let s = Session {
            version: STATE_VERSION,
            config_path: cfg.to_path_buf(),
            services: vec![SessionService {
                name: "web".into(),
                pid: 1234,
                log: session_dir(cfg).join("web.log"),
            }],
        };
        save(cfg, &s).unwrap();
        let loaded = load(cfg).expect("load");
        assert_eq!(loaded.services.len(), 1);
        assert_eq!(loaded.services[0].name, "web");
        clear(cfg);
        assert!(load(cfg).is_none());
        std::fs::remove_dir_all(session_dir(cfg)).ok();
    }
}

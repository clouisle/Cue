//! 后台会话状态：cache 目录定位、state.json 读写、pid 存活检测。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const STATE_VERSION: u32 = 2;
const STATE_FILE: &str = "state.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionService {
    pub name: String,
    pub pid: u32,
    /// Windows 进程创建时间（FILETIME ticks）。用于拒绝 PID 复用；Unix 为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_start: Option<u64>,
    /// 该服务的日志文件（append，原始输出，无前缀）。
    pub log: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub config_path: PathBuf,
    pub services: Vec<SessionService>,
}

/// 会话目录：`$XDG_CACHE_HOME/cue/<配置路径hash>`，
/// fallback `~/.cache/cue/<hash>`（macOS 同）。
pub fn session_dir(config_path: &Path) -> PathBuf {
    let mut h = DefaultHasher::new();
    let canon = config_path.canonicalize().unwrap_or_else(|_| config_path.to_path_buf());
    canon.hash(&mut h);
    cache_root().join(format!("{:016x}", h.finish()))
}

fn cache_root() -> PathBuf {
    #[cfg(windows)]
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local_app_data).join("cue");
    }
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(x).join("cue");
    }
    #[cfg(unix)]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".cache").join("cue");
    }
    std::env::temp_dir().join("cue")
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

/// pid 存活检测。Unix 使用 `kill(pid, 0)`；Windows 查询退出状态。
#[cfg(unix)]
pub fn is_alive(pid: u32) -> bool {
    unsafe {
        libc::kill(pid as i32, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(windows)]
pub fn is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut exit_code = 0;
        let alive = GetExitCodeProcess(handle, &mut exit_code) != 0 && exit_code == 259;
        let _ = CloseHandle(handle);
        alive
    }
}

/// 捕获 Windows 进程创建时间；Unix 不需要额外的 PID 身份字段。
#[cfg(windows)]
pub fn process_start(pid: u32) -> Option<u64> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let ok = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) != 0;
        let _ = CloseHandle(handle);
        ok.then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
}


/// 判断记录的服务是否仍是原始进程。Windows 创建时间不匹配时视作已退出，避免 PID 复用误杀。
#[cfg(windows)]
pub fn is_service_alive(service: &SessionService) -> bool {
    is_alive(service.pid)
        && service
            .process_start
            .is_some_and(|started| process_start(service.pid) == Some(started))
}

#[cfg(not(windows))]
pub fn is_service_alive(service: &SessionService) -> bool {
    is_alive(service.pid)
}

impl SessionService {
    /// 构造可安全持久化的服务记录。Windows 必须取得创建时间，缺失则不能托管该进程。
    #[cfg(windows)]
    pub fn new(name: String, pid: u32, log: PathBuf) -> Result<Self, String> {
        let process_start = process_start(pid).ok_or_else(|| {
            format!("cannot read creation time for Windows service '{name}' (pid {pid})")
        })?;
        Ok(Self { name, pid, process_start: Some(process_start), log })
    }

    #[cfg(not(windows))]
    pub fn new(name: String, pid: u32, log: PathBuf) -> Result<Self, String> {
        Ok(Self { name, pid, process_start: None, log })
    }
}

/// 服务在 `timeout` 内退出则返回 true（轮询 50ms）。
pub fn wait_service_exited(service: &SessionService, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !is_service_alive(service) {
            return true;
        }
        if Instant::now() >= deadline {
            return !is_service_alive(service);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

impl Session {
    /// 是否有至少一个服务存活。
    pub fn is_any_running(&self) -> bool {
        self.services.iter().any(is_service_alive)
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
        let a = Path::new("/tmp/proj/.cue.json");
        let b = Path::new("/tmp/other/.cue.json");
        let d1 = session_dir(a);
        let d2 = session_dir(a);
        assert_eq!(d1, d2);
        assert_ne!(d1, session_dir(b));
    }

    #[test]
    fn save_load_roundtrip() {
        let cfg = Path::new("/tmp/proj/.cue.json");
        let s = Session {
            version: STATE_VERSION,
            config_path: cfg.to_path_buf(),
            services: vec![SessionService {
                name: "web".into(),
                pid: 1234,
                process_start: None,
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

    #[cfg(windows)]
    #[test]
    fn service_identity_rejects_reused_pid() {
        let pid = std::process::id();
        let started = process_start(pid).expect("current process creation time");
        let service = SessionService::new("self".into(), pid, PathBuf::new())
            .expect("current process identity");
        assert!(is_service_alive(&service));
        let reused = SessionService { process_start: Some(started + 1), ..service };
        assert!(!is_service_alive(&reused));
    }
}

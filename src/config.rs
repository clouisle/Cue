//! `.yun-dev.json` 模型、自动发现与校验。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const CONFIG_FILE_NAME: &str = ".yun-dev.json";

/// 从 `start` 目录开始向上逐级查找最近的配置文件（类似 git 仓库发现）。
pub fn discover(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(CONFIG_FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    #[default]
    No,
    Always,
    OnFailure,
}

impl RestartPolicy {
    /// 进程退出后是否按策略重启（停机流程中调用方会先置 shutdown）。
    pub fn should_restart(&self, exit_success: bool) -> bool {
        match self {
            RestartPolicy::No => false,
            RestartPolicy::Always => true,
            RestartPolicy::OnFailure => !exit_success,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Service {
    /// 经系统 shell 执行的命令（unix `sh -c` / windows `cmd /C`）。
    #[serde(default)]
    pub command: Option<String>,
    /// 直接 exec 的程序；与 `command` 二选一。
    #[serde(default)]
    pub program: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// 相对配置文件所在目录；缺省为配置文件目录。
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// 追加覆盖到继承的环境变量。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub restart: RestartPolicy,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub services: BTreeMap<String, Service>,
}

impl Config {
    /// 读取并校验配置文件；错误消息带路径上下文。
    pub fn load(path: &Path) -> Result<Config, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let cfg: Config = serde_json::from_str(&raw)
            .map_err(|e| format!("invalid {}: {e}", path.display()))?;
        cfg.validate(path)?;
        Ok(cfg)
    }

    /// 逐服务校验：command / program 至少提供一个。
    pub fn validate(&self, path: &Path) -> Result<(), String> {
        for (name, svc) in &self.services {
            if svc.command.is_none() && svc.program.is_none() {
                return Err(format!(
                    "{}: service '{name}' needs 'command' or 'program'",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    /// 所有服务名中最长宽度，用于日志前缀对齐。
    pub fn max_name_width(&self) -> usize {
        self.services.keys().map(|k| k.len()).max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_config(dir: &Path, body: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let p = dir.join(CONFIG_FILE_NAME);
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn parses_full_config_with_defaults() {
        let dir = std::env::temp_dir().join(format!("ydev-test-{}", std::process::id()));
        let p = write_config(
            &dir,
            r#"{
                "services": {
                    "frontend": { "command": "bun run dev", "cwd": "web", "env": {"PORT": "3000"} },
                    "backend": { "program": "cargo", "args": ["run"] }
                }
            }"#,
        );
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.services.len(), 2);
        let fe = &cfg.services["frontend"];
        assert_eq!(fe.command.as_deref(), Some("bun run dev"));
        assert_eq!(fe.cwd.as_deref(), Some(Path::new("web")));
        assert_eq!(fe.env["PORT"], "3000");
        assert_eq!(fe.restart, RestartPolicy::No);
        let be = &cfg.services["backend"];
        assert_eq!(be.program.as_deref(), Some("cargo"));
        assert_eq!(be.args, vec!["run"]);
        assert_eq!(be.restart, RestartPolicy::No);
        assert_eq!(cfg.max_name_width(), 8);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_command_is_error() {
        let dir = std::env::temp_dir().join(format!("ydev-test2-{}", std::process::id()));
        let p = write_config(&dir, r#"{"services": {"x": {}}}"#);
        let err = Config::load(&p).unwrap_err();
        assert!(err.contains("needs 'command' or 'program'"), "{err}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bad_json_is_error() {
        let dir = std::env::temp_dir().join(format!("ydev-test3-{}", std::process::id()));
        let p = write_config(&dir, "{not json");
        let err = Config::load(&p).unwrap_err();
        assert!(err.contains("invalid"), "{err}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discovers_nearest_upward() {
        let dir = std::env::temp_dir().join(format!("ydev-test4-{}", std::process::id()));
        write_config(&dir, r#"{"services": {}}"#);
        let deep = dir.join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        let found = discover(&deep).unwrap();
        assert_eq!(found, dir.join(CONFIG_FILE_NAME));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discover_returns_none_when_absent() {
        let dir = std::env::temp_dir().join(format!("ydev-test5-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert!(discover(&dir).is_none());
        fs::remove_dir_all(&dir).ok();
    }
}

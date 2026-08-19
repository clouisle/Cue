//! `.yun-dev.json` 模型、自动发现与校验。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    /// KEY=VALUE 文件（字符串或数组），相对配置文件目录；env 字段覆盖其值。
    #[serde(default)]
    pub env_file: Option<EnvFile>,
    #[serde(default)]
    pub restart: RestartPolicy,
    /// 启动前置依赖；本服务在其全部依赖就绪前不启动。
    #[serde(default)]
    pub depends_on: Option<DependsOn>,
    /// 健康检查；被依赖方要求 service_healthy 时在预算内轮询。
    #[serde(default)]
    pub healthcheck: Option<Healthcheck>,
}

impl Service {
    /// 规范化依赖条件：数组简写 `["db"]` 等价 `{"db": {"condition": "service_started"}}`。
    pub fn depends_on_conditions(&self) -> BTreeMap<String, DepCondition> {
        match &self.depends_on {
            None => BTreeMap::new(),
            Some(DependsOn::List(v)) => v
                .iter()
                .map(|s| (s.clone(), DepCondition::Started))
                .collect(),
            Some(DependsOn::Map(m)) => m.clone(),
        }
    }
}

/// 依赖条件（对齐 compose 命名，形式为 `{"condition": "service_healthy"}`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DepCondition {
    /// 进程已 spawn 即视为就绪。
    Started,
    /// 健康检查通过才算就绪。
    Healthy,
    /// 进程以退出码 0 结束才算就绪（一次性任务，如迁移）。
    Completed,
}

impl<'de> Deserialize<'de> for DepCondition {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct CondVisitor;
        impl<'de> Visitor<'de> for CondVisitor {
            type Value = DepCondition;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a map with a \"condition\" key")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut cond = None;
                while let Some((k, v)) = map.next_entry::<String, String>()? {
                    if k == "condition" {
                        cond = Some(v);
                    }
                }
                match cond.as_deref() {
                    Some("service_started") => Ok(DepCondition::Started),
                    Some("service_healthy") => Ok(DepCondition::Healthy),
                    Some("service_completed_successfully") => Ok(DepCondition::Completed),
                    Some(other) => Err(A::Error::custom(format!("unknown condition '{other}'"))),
                    None => Err(A::Error::custom("missing \"condition\" key")),
                }
            }
        }

        d.deserialize_map(CondVisitor)
    }
}

impl DepCondition {
    /// 取更严条件：healthy > completed > started。
    fn stricter(self, other: DepCondition) -> DepCondition {
        use DepCondition::*;
        match (self, other) {
            (Healthy, _) | (_, Healthy) => Healthy,
            (Completed, _) | (_, Completed) => Completed,
            _ => Started,
        }
    }
}

/// `depends_on` 两种写法：数组简写或显式条件 map。
/// untagged derive 对 seq/map 互不回溯（serde 已知缺陷），故手写 Deserialize。
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum DependsOn {
    List(Vec<String>),
    Map(BTreeMap<String, DepCondition>),
}

impl<'de> Deserialize<'de> for DependsOn {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, SeqAccess, Visitor};

        struct DependsOnVisitor;
        impl<'de> Visitor<'de> for DependsOnVisitor {
            type Value = DependsOn;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an array of service names or a map of dependency conditions")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut v = Vec::new();
                while let Some(s) = seq.next_element::<String>()? {
                    v.push(s);
                }
                Ok(DependsOn::List(v))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut m = BTreeMap::new();
                while let Some((k, c)) = map.next_entry::<String, DepCondition>()? {
                    m.insert(k, c);
                }
                Ok(DependsOn::Map(m))
            }
        }

        d.deserialize_any(DependsOnVisitor)
    }
}

fn default_hc_interval() -> Duration {
    Duration::from_secs(2)
}

fn default_hc_timeout() -> Duration {
    Duration::from_secs(5)
}

fn default_hc_retries() -> u32 {
    30
}

fn default_hc_start_period() -> Duration {
    Duration::ZERO
}

/// 健康检查。默认值取 dev 取向（interval 2s / timeout 5s / retries 30），与 compose 不同。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Healthcheck {
    pub test: String,
    #[serde(default = "default_hc_interval", deserialize_with = "de_duration")]
    pub interval: Duration,
    #[serde(default = "default_hc_timeout", deserialize_with = "de_duration")]
    pub timeout: Duration,
    #[serde(default = "default_hc_retries")]
    pub retries: u32,
    #[serde(default = "default_hc_start_period", deserialize_with = "de_duration")]
    pub start_period: Duration,
}

impl Healthcheck {
    /// 总等待预算 = start_period + interval × retries。
    pub fn budget(&self) -> Duration {
        self.start_period + self.interval * self.retries.max(1)
    }
}

fn de_duration<'de, D>(d: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    parse_duration(&s).map_err(<D::Error as serde::de::Error>::custom)
}

/// `env_file`：字符串或路径数组。手写 Deserialize 避免 untagged 容器回溯缺陷。
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum EnvFile {
    One(String),
    Many(Vec<String>),
}

impl EnvFile {
    pub fn files(&self) -> Vec<&str> {
        match self {
            EnvFile::One(f) => vec![f.as_str()],
            EnvFile::Many(v) => v.iter().map(|s| s.as_str()).collect(),
        }
    }
}

impl<'de> Deserialize<'de> for EnvFile {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{SeqAccess, Visitor};

        struct EnvFileVisitor;
        impl<'de> Visitor<'de> for EnvFileVisitor {
            type Value = EnvFile;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a file path or an array of file paths")
            }

            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Self::Value, E> {
                Ok(EnvFile::One(s.to_string()))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut v = Vec::new();
                while let Some(s) = seq.next_element::<String>()? {
                    v.push(s);
                }
                Ok(EnvFile::Many(v))
            }
        }

        d.deserialize_any(EnvFileVisitor)
    }
}

/// 解析 KEY=VALUE 文件内容：`#` 注释、空行、无值 KEY、首尾引号剥离。
pub fn parse_env_file(content: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim();
            if !key.is_empty() {
                map.insert(key.to_string(), unquote(v.trim()));
            }
        } else {
            // 无 `=`：KEY 视为空值。
            map.insert(line.to_string(), String::new());
        }
    }
    map
}

fn unquote(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
        || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// 变量插值：`${VAR}` / `${VAR:-默认}` / `${VAR-默认}` / `$$` 转义字面 `$`。
/// 裸 `$VAR` 原样保留——交给运行时 shell 展开（命令中的 `$i`、`$HOME` 等
/// shell 变量不被吞掉；与 compose 不同，后者会插值裸形式）。
pub fn interpolate(input: &str, vars: &BTreeMap<String, String>) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '$' || i + 1 >= chars.len() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        if chars[i + 1] == '$' {
            out.push('$');
            i += 2;
            continue;
        }
        if chars[i + 1] == '{' {
            let rest = &chars[i + 2..];
            match rest.iter().position(|&c| c == '}') {
                Some(rel) => {
                    let inner: String = rest[..rel].iter().collect();
                    out.push_str(&resolve_var(&inner, vars));
                    i += 2 + rel + 1;
                }
                None => {
                    out.push(chars[i]);
                    i += 1;
                }
            }
        } else {
            // 裸 $VAR：原样保留（运行时 shell 展开）。
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn resolve_var(inner: &str, vars: &BTreeMap<String, String>) -> String {
    if let Some((name, default)) = inner.split_once(":-") {
        let v = vars.get(name.trim()).map(String::as_str).unwrap_or("");
        if v.is_empty() {
            default.to_string()
        } else {
            v.to_string()
        }
    } else if let Some((name, default)) = inner.split_once('-') {
        vars.get(name.trim())
            .cloned()
            .unwrap_or_else(|| default.to_string())
    } else {
        vars.get(inner.trim()).cloned().unwrap_or_default()
    }
}

/// 插值变量表：shell 环境优先，配置目录 `.env` 兜底。
fn env_vars_with_dotenv(config_dir: &Path) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    if let Ok(content) = std::fs::read_to_string(config_dir.join(".env")) {
        vars.extend(parse_env_file(&content));
    }
    for (k, v) in std::env::vars() {
        vars.insert(k, v);
    }
    vars
}

/// 解析 compose 风格时长，支持组合："500ms"、"2s"、"1m30s"、"1h"。
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let chars: Vec<char> = s.trim().chars().collect();
    let mut total = Duration::ZERO;
    let mut num = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            i += 1;
            continue;
        }
        if num.is_empty() {
            return Err(format!("bad duration '{s}'"));
        }
        let v: f64 = num
            .parse()
            .map_err(|_| format!("bad number in duration '{s}'"))?;
        let (unit_len, secs) = match c {
            'h' => (1, v * 3600.0),
            'm' if i + 1 < chars.len() && chars[i + 1] == 's' => (2, v / 1000.0),
            'm' => (1, v * 60.0),
            's' => (1, v),
            _ => return Err(format!("bad unit in duration '{s}'")),
        };
        total += Duration::from_secs_f64(secs);
        i += unit_len;
        num.clear();
    }
    if !num.is_empty() {
        return Err(format!("missing unit in duration '{s}'"));
    }
    if total.is_zero() {
        return Err(format!("bad duration '{s}'"));
    }
    Ok(total)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub services: BTreeMap<String, Service>,
}

impl Config {
    /// 读取、插值并校验配置文件；错误消息带路径上下文。
    pub fn load(path: &Path) -> Result<Config, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let mut cfg: Config = serde_json::from_str(&raw)
            .map_err(|e| format!("invalid {}: {e}", path.display()))?;
        let vars = env_vars_with_dotenv(path.parent().unwrap_or(Path::new(".")));
        cfg.resolve(&vars);
        cfg.validate(path)?;
        Ok(cfg)
    }

    /// 对 command / program / args / cwd / env 值 / healthcheck.test 应用变量插值。
    pub fn resolve(&mut self, vars: &BTreeMap<String, String>) {
        for svc in self.services.values_mut() {
            svc.command = svc.command.as_deref().map(|s| interpolate(s, vars));
            svc.program = svc.program.as_deref().map(|s| interpolate(s, vars));
            for a in &mut svc.args {
                *a = interpolate(a, vars);
            }
            svc.cwd = svc.cwd.as_deref().and_then(|p| {
                p.to_str().map(|s| PathBuf::from(interpolate(s, vars)))
            });
            for v in svc.env.values_mut() {
                *v = interpolate(v, vars);
            }
            if let Some(hc) = &mut svc.healthcheck {
                hc.test = interpolate(&hc.test, vars);
            }
        }
    }

    /// 逐服务校验：command/program 至少一个；依赖存在且无环。
    pub fn validate(&self, path: &Path) -> Result<(), String> {
        for (name, svc) in &self.services {
            if svc.command.is_none() && svc.program.is_none() {
                return Err(format!(
                    "{}: service '{name}' needs 'command' or 'program'",
                    path.display()
                ));
            }
        }
        self.levels().map_err(|e| format!("{}: {e}", path.display()))?;
        for (name, cond) in &self.required_conditions() {
            if *cond == DepCondition::Healthy {
                let svc = &self.services[name];
                if svc.healthcheck.is_none() {
                    return Err(format!(
                        "{}: service '{name}' is depended on with service_healthy but has no healthcheck",
                        path.display()
                    ));
                }
            }
        }
        Ok(())
    }

    /// 依赖图拓扑分层：无依赖的服务在第 0 波，依赖深度 +1 依次后推；
    /// 同层服务并行启动。环或未知依赖报错。
    pub fn levels(&self) -> Result<Vec<Vec<String>>, String> {
        use std::collections::HashMap;

        #[derive(Clone, Copy, PartialEq)]
        enum Color {
            White,
            Gray,
            Black,
        }

        fn visit(
            name: &str,
            cfg: &Config,
            color: &mut HashMap<String, Color>,
            depth: &mut HashMap<String, usize>,
        ) -> Result<usize, String> {
            match color.get(name).copied().unwrap_or(Color::White) {
                Color::Black => return Ok(depth[name]),
                Color::Gray => return Err(format!("dependency cycle involving '{name}'")),
                Color::White => {}
            }
            color.insert(name.to_string(), Color::Gray);
            let mut d = 0;
            for dep in cfg.deps_of(name).keys() {
                if !cfg.services.contains_key(dep) {
                    return Err(format!("service '{name}' depends on unknown service '{dep}'"));
                }
                d = d.max(visit(dep, cfg, color, depth)? + 1);
            }
            color.insert(name.to_string(), Color::Black);
            depth.insert(name.to_string(), d);
            Ok(d)
        }

        let mut color: HashMap<String, Color> = HashMap::new();
        let mut depth: HashMap<String, usize> = HashMap::new();
        let mut by_depth: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for name in self.services.keys() {
            let d = visit(name, self, &mut color, &mut depth)?;
            by_depth.entry(d).or_default().push(name.clone());
        }
        Ok(by_depth.into_values().collect())
    }

    /// 服务的规范化依赖（BTreeMap 保序）。
    pub fn deps_of(&self, name: &str) -> BTreeMap<String, DepCondition> {
        self.services
            .get(name)
            .map(|s| s.depends_on_conditions())
            .unwrap_or_default()
    }

    /// 每个服务作为依赖需满足的最严条件：healthy > completed > started。
    pub fn required_conditions(&self) -> BTreeMap<String, DepCondition> {
        let mut req: BTreeMap<String, DepCondition> = BTreeMap::new();
        for svc in self.services.values() {
            for (dep, cond) in svc.depends_on_conditions() {
                let cur = req.entry(dep).or_insert(DepCondition::Started);
                *cur = cur.stricter(cond);
            }
        }
        req
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

    #[test]
    fn parse_duration_units_and_composition() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_duration("1m30s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("1.5s").unwrap(), Duration::from_millis(1500));
        assert!(parse_duration("2").is_err());
        assert!(parse_duration("xs").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn depends_on_both_forms_and_healthcheck_defaults() {
        let dir = std::env::temp_dir().join(format!("ydev-test6-{}", std::process::id()));
        let p = write_config(
            &dir,
            r#"{
                "services": {
                    "db": { "command": "x", "healthcheck": { "test": "true" } },
                    "web": { "command": "y", "depends_on": ["db"] },
                    "api": {
                        "command": "z",
                        "depends_on": { "db": { "condition": "service_healthy" } }
                    }
                }
            }"#,
        );
        let cfg = Config::load(&p).unwrap();
        // 数组简写 → started；map → healthy
        assert_eq!(cfg.deps_of("web")["db"], DepCondition::Started);
        assert_eq!(cfg.deps_of("api")["db"], DepCondition::Healthy);
        // 最严合并：web 要 started、api 要 healthy → db 需 healthy
        let req = cfg.required_conditions();
        assert_eq!(req["db"], DepCondition::Healthy);
        // healthcheck 默认值
        let hc = cfg.services["db"].healthcheck.as_ref().unwrap();
        assert_eq!(hc.interval, Duration::from_secs(2));
        assert_eq!(hc.timeout, Duration::from_secs(5));
        assert_eq!(hc.retries, 30);
        assert_eq!(hc.start_period, Duration::ZERO);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn levels_chain_and_parallel_waves() {
        let dir = std::env::temp_dir().join(format!("ydev-test7-{}", std::process::id()));
        let p = write_config(
            &dir,
            r#"{
                "services": {
                    "a": { "command": "x" },
                    "b": { "command": "x", "depends_on": ["a"] },
                    "c": { "command": "x", "depends_on": ["a"] },
                    "d": { "command": "x", "depends_on": ["b", "c"] }
                }
            }"#,
        );
        let cfg = Config::load(&p).unwrap();
        let levels = cfg.levels().unwrap();
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0], vec!["a"]);
        assert_eq!(levels[1], vec!["b", "c"]);
        assert_eq!(levels[2], vec!["d"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dependency_cycle_and_unknown_dep_are_errors() {
        let dir = std::env::temp_dir().join(format!("ydev-test8-{}", std::process::id()));
        let p = write_config(
            &dir,
            r#"{
                "services": {
                    "a": { "command": "x", "depends_on": ["b"] },
                    "b": { "command": "x", "depends_on": ["a"] }
                }
            }"#,
        );
        let err = Config::load(&p).unwrap_err();
        assert!(err.contains("cycle"), "{err}");

        let p2 = write_config(&dir, r#"{"services": {"a": {"command": "x", "depends_on": ["ghost"]}}}"#);
        let err2 = Config::load(&p2).unwrap_err();
        assert!(err2.contains("unknown service 'ghost'"), "{err2}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn interpolate_supports_all_syntaxes() {
        let mut vars = BTreeMap::new();
        vars.insert("PORT".to_string(), "8080".to_string());
        vars.insert("EMPTY".to_string(), String::new());

        assert_eq!(interpolate("a${PORT}b", &vars), "a8080b");
        assert_eq!(interpolate("$PORT", &vars), "$PORT", "裸 $VAR 保留给运行时 shell");
        assert_eq!(interpolate("${MISSING}", &vars), "");
        assert_eq!(interpolate("${MISSING:-def}", &vars), "def");
        assert_eq!(interpolate("${EMPTY:-def}", &vars), "def", ":- 覆盖空值");
        assert_eq!(interpolate("${EMPTY-def}", &vars), "", "- 不覆盖空值");
        assert_eq!(interpolate("${PORT:-def}", &vars), "8080");
        assert_eq!(interpolate("no vars here", &vars), "no vars here");
        assert_eq!(interpolate("$${PORT}", &vars), "${PORT}", "$$ 转义为字面 $");
        assert_eq!(interpolate("${UNCLOSED", &vars), "${UNCLOSED");
    }

    #[test]
    fn parse_env_file_handles_comments_quotes_and_empty() {
        let content = "# comment\n\nKEY=value\nQUOTED=\"a b\"\nSINGLE='x'\nBARE\nEMPTY=\n";
        let map = parse_env_file(content);
        assert_eq!(map["KEY"], "value");
        assert_eq!(map["QUOTED"], "a b");
        assert_eq!(map["SINGLE"], "x");
        assert_eq!(map["BARE"], "");
        assert_eq!(map["EMPTY"], "");
        assert!(!map.contains_key("# comment"));
    }

    #[test]
    fn load_applies_dotenv_interpolation_and_env_file() {
        let dir = std::env::temp_dir().join(format!("ydev-test9-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(".env"),
            "PORT=8080\nTOKEN=secret\n",
        )
        .unwrap();
        fs::write(
            dir.join("svc.env"),
            "FROM_FILE=yes\n",
        )
        .unwrap();
        let p = write_config(
            &dir,
            r#"{
                "services": {
                    "web": {
                        "command": "echo ${PORT} ${TOKEN} ${MISSING:-none}",
                        "env_file": "svc.env",
                        "env": { "URL": "http://x:${PORT}" }
                    }
                }
            }"#,
        );
        let cfg = Config::load(&p).unwrap();
        let web = &cfg.services["web"];
        assert_eq!(web.command.as_deref(), Some("echo 8080 secret none"));
        assert_eq!(web.env["URL"], "http://x:8080");
        assert!(matches!(web.env_file, Some(EnvFile::One(ref f)) if f == "svc.env"));
        fs::remove_dir_all(&dir).ok();
    }
}

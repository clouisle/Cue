# Cue 设计文档

## Background & Goals

### 问题
开发时一个项目往往要同时启动前端、后端、worker 等多个服务，目前的做法是每个服务开一个终端手动运行：
- 终端窗口多、难以管理
- 各服务日志分散，无法横向对比
- 停止时容易残留孤儿进程

### 目标
- 在项目目录下放置 `.cue.json`，一条命令（`cue up`）启动全部服务
- 自动发现：从当前目录向上逐级查找最近的 `.cue.json`（类似 git 仓库发现）
- 日志输出像 docker compose：服务名彩色前缀、多服务交错行流、退出码提示、优雅停机
- 一个终端完成启动/观察/停止全流程

### 成功标准
- `up` 一次拉起所有服务，日志带 `name  | ` 前缀且着色
- 服务自然退出时打印 `name exited with code N`，全部退出后工具以正确退出码结束
- Ctrl+C 一次优雅停止（macOS/Linux：SIGTERM → SIGKILL；Windows：Ctrl+Break → taskkill），两次强制立即停止
- 部分行（无换行结尾的输出）在进程退出时被冲刷出来，不丢失

## High-Level Design

### 模块划分

```
src/
  main.rs    CLI 入口（clap derive）：up(-d, service...) / restart / down / logs / ps / config / validate；信号处理与退出码汇总
  config.rs  .cue.json 模型（serde）、自动发现（向上逐级查找）、校验、服务选择依赖闭包
  session.rs 后台会话状态文件（cache 目录、pid/创建时间/日志路径记录、存活检测）
  runner.rs  服务进程 spawn、日志行流读取与打印、重启策略、跨平台进程组控制
  term.rs    ANSI 颜色调色板、前缀格式化
```

### 数据流

```
前台 up:
main: 解析 CLI → 加载配置 → 解析指定服务及其传递依赖 → 拓扑分层（levels）→ 逐波次触发服务 task 的启动闸门
每个服务 task: 等闸门 → 校验依赖就绪 → spawn 子进程(新进程组) → 双 reader(stdout/stderr) → mpsc 行流 → 打印
  首轮 spawn 后按最严依赖条件上报就绪：started 立即 / healthy 轮询通过 / completed 退出码 0
  main 收齐本波次就绪(或失败)后触发下一波次闸门；失败传播：依赖方跳过启动
main: 全部波次启动完进入 select { Ctrl+C, 全部服务退出 }
  Ctrl+C → 置 shutdown 标志 → Unix：进程组 SIGTERM → SIGKILL；Windows：控制台组 Ctrl+Break → taskkill /F /T
  全部退出 → 汇总退出码（任一服务非零码退出=1，否则 0）

后台 up -d / restart / down / logs / ps:
up -d:   按所选服务及其依赖的波次 spawn（stdout/stderr → cache 日志文件）→ 同步等待本波次就绪 → 写状态文件 → 退出
restart: 停止会话内指定（或全部）服务 → 逐个用当前配置和原日志文件重启 → 每次替换状态中的 pid
ps:      读状态文件 → 按 pid 存活检测 → 打印 running/exited 表
logs:    读状态文件 → 按需 dump / `-f` 轮询增量 → 打印带服务名前缀
down:    读状态文件 → Unix：进程组 SIGTERM → SIGKILL；Windows：控制台组 Ctrl+Break → taskkill /F /T → 删状态文件

### 配置格式 `.cue.json`

```json
{
  "services": {
    "db": {
      "command": "pg_ctl start",
      "healthcheck": { "test": "pg_isready -h localhost -p 5432", "interval": "2s", "retries": 30 }
    },
    "backend": {
      "command": "cargo run",
      "depends_on": { "db": { "condition": "service_healthy" } },
      "restart": "on-failure"
    },
    "frontend": {
      "command": "bun run dev --port ${PORT}",
      "cwd": "web",
      "env_file": "web/.env",
      "env": { "API_URL": "http://localhost:${PORT}" },
      "depends_on": ["backend"]
    }
  }
}
```

字段语义：
- `services`: map，name → 服务定义。name 同时是日志前缀
- `command`: 字符串，经系统 shell 执行（macOS/Linux `sh -c` / Windows `cmd /C`）；跨平台配置推荐 `program` + `args`
- `program` + `args`: 直接 exec，不经过 shell；与 `command` 二选一（都缺省=校验错误）
- `cwd`: 相对配置文件所在目录（非当前目录），缺省=配置文件目录
- `env`: 追加覆盖到继承的环境变量
- `env_file`: 字符串或数组，加载 KEY=VALUE 文件注入进程环境（`#` 注释、空行、无值 KEY、引号剥离）；优先级：继承 < env_file < env
- 变量插值：配置内 `${VAR}` / `${VAR:-默认}` / `${VAR-默认}`，应用于 command / program / args / cwd / env 值 / healthcheck.test；查找顺序 shell 环境 > 配置文件目录 `.env` > 默认值；`env_file` 文件不参与插值（仅注入进程）
- 裸 `$VAR` 原样保留给运行时 shell 展开（命令中的 `$i`、`$HOME` 不被吞；与 compose 的"插值裸形式"有意不同）；`$$` 转义为字面 `$`
- `restart`: `no`（默认）| `always` | `on-failure`，重启间隔 1s，停机流程中不重启
- `depends_on`: 数组简写 `["db"]`（等价 `service_started`）或 map 形式 `{"db": {"condition": "service_healthy" | "service_started" | "service_completed_successfully"}}`；依赖服务就绪前本服务不启动，按最长依赖链分层（波次）并行启动
- `healthcheck`: `{"test": 命令, "interval": "2s", "timeout": "5s", "retries": 30, "start_period": "0s"}`；test 退出 0 = 健康。默认值与 compose 不同（dev 取向：interval 2s / timeout 5s / retries 30），等待预算 = start_period + interval × retries，预算内成功一次即就绪。时间格式支持 `500ms`/`2s`/`1m30s` 组合

## Implementation Plan

### Stage 1: 依赖与骨架
- **Files modified**: `Cargo.toml`, `src/term.rs`
- **Specific logic**:
  - `tokio`(rt-multi-thread/macros/process/io-util/signal/time)、`serde`(derive)、`serde_json`、`clap`(derive)；Unix 下 `libc`（进程组信号），Windows 下 `windows-sys`（控制台事件与进程身份）
  - `term.rs`: 8 色调色板按服务序取色；`paint(code, s)` 输出 ANSI；颜色开关由调用方决定（TTY && !NO_COLOR && !--no-color）
- **Validation**: `cargo build` 通过

### Stage 2: config.rs 解析与发现
- **Files modified**: `src/config.rs`
- **Specific logic**:
  - `Config { services: BTreeMap<String, Service> }`；`Service { command?, program?, args, cwd?, env, restart }`，`restart` 默认 `No`
  - `discover(start: &Path) -> Option<PathBuf>`: 从 start 逐级向父目录找 `.cue.json`
  - `load(path) -> Result<Config, String>`: 读文件 → serde_json 解析 → 逐服务校验（command/program 至少一个）
- **Validation**: 单元测试：默认值、缺 command 报错、坏 JSON 报错、向上发现最近配置

### Stage 3: runner.rs 进程编排与日志流
- **Files modified**: `src/runner.rs`
- **Specific logic**:
  - `spawn_service`: 构造 `tokio::process::Command`；Unix 下 `process_group(0)`，Windows 下 `CREATE_NEW_PROCESS_GROUP`，两者都让服务树拥有独立停止域；`cwd` 相对配置目录解析；stdin 置 null、stdout/stderr piped
  - `read_lines`: `read_until(b'\n')` 字节级行读，`from_utf8_lossy` 解码；EOF 时残留字节也发一行（冲刷部分行）
  - `run_service`: spawn 后双 reader → 每服务一个 mpsc 行流；`select { child.wait(), line }` 边跑边打印；退出后打印 `exited with code N`；按 `restart` 策略与 shutdown 标志决定是否 1s 后重启
  - 行打印：`{name填充}  | {text}`，name 用调色板第 (服务序 % 8) 色；退出/重启消息 dim 灰
- **Validation**: 单元测试：duplex 流喂"abc"无换行 → EOF 冲刷出整行；spawn 真实 `sh -c` 进程验证行流

### Stage 4: main.rs CLI 与信号处理
- **Files modified**: `src/main.rs`
- **Specific logic**:
  - clap: `up`（默认，`--stop-timeout` 默认 10s）/ `config`（打印解析结果）/ `validate`（仅校验）；`--file` 指定配置文件；`--no-color`
  - up 流程：加载配置 → 全部 spawn 后进入 `select { ctrl_c, 全部退出 }`
  - 优雅停机：Unix 向进程组发 SIGTERM、超时 SIGKILL；Windows 向控制台进程组发 Ctrl+Break、超时 `taskkill /F /T`；`--stop-timeout 0` 立即强制。两次 Ctrl+C 直接强制。
  - 退出码：任一服务以非零码退出（自然退出或 Ctrl+C 停机后）→ 1；停机时被信号终止的服务不计失败；否则 0
- **Validation**: 集成测试（见 Stage 5）

### Stage 5: 集成测试与冒烟
- **Files modified**: `tests/`, `testdata/`
- **Specific logic**:
  - fixture 项目含 `.cue.json`：frontend（打印若干行后退出 0）、backend（无限循环，用于 SIGINT 停机验证）、worker（`exit 3` 验证失败码汇总）
  - 集成测试：以 fixture 为 cwd 启动二进制 → 断言输出含 `frontend  | ` 前缀 → 对二进制发 SIGINT → 断言优雅退出且退出码为 1（worker 失败）且输出含 `exited with code`
  - 冒烟：真实终端手动运行一次观察配色
- **Validation**: `cargo test` 全绿；`cargo clippy -- -D warnings` 通过

### Stage 6: 后台模式（up -d / down / logs / ps）
- **Files modified**: `src/session.rs`（新增）、`src/runner.rs`、`src/main.rs`
- **Specific logic**:
  - 状态文件：Unix 为 `$XDG_CACHE_HOME/cue/<配置路径hash>/state.json`（fallback `~/.cache`），Windows 为 `%LOCALAPPDATA%\\cue/<配置路径hash>/state.json`；每服务记录 `{name, pid, process_start?, log}`，Windows 创建时间防 PID 复用误杀。
  - `up -d [service...]`: 复用 `launch_spec`（command/program 二选一，与前台共享）→ 仅启动所选服务及其传递依赖，按波次等待就绪后写状态；stdout/stderr → 日志文件；状态写失败则强制终止已起服务并报错；打印提示（logs/ps/down 用法）；任一 spawn 失败 → 1
  - 前后台互斥：前台 `up` 与 `up -d` 启动前检测 state 中仍属于本会话的存活服务 → 有则报错提示先 `down`
  - `down`: Unix 对运行中进程组 SIGTERM → 等待 → SIGKILL；Windows Ctrl+Break → 等待 → `taskkill /F /T`；删状态文件（日志文件保留）
  - `ps`: 表格输出 name / pid / running|exited / log 路径；Unix 用 `kill(pid, 0)`，Windows 用 `GetExitCodeProcess` + 创建时间核对
  - `logs [-f|--follow] [--tail N] [service...]`: 默认 dump 全部历史；`-f`/`--follow` 每 200ms 轮询增量（记录字节偏移）；`--tail N` 从倒数第 N 行开始；每行带服务名前缀。全局配置路径仅用 `--file`。
  - 后台 spawn 用 std::process（Child drop 不杀进程，工具退出后服务继续跑）；`runner::print_line` 提为 pub 供 logs 复用
- **Validation**: 集成测试：up -d → ps 显示 running → logs 含前缀历史 → -f 捕获追加行 → 重复 up -d 报错 → 前台 up 报错 → down 清 state

### Stage 7: 依赖编排（depends_on + healthcheck）
- **Files modified**: `src/config.rs`、`src/runner.rs`、`src/main.rs`
- **Specific logic**:
  - config: `DependsOn`（untagged：数组=started / map=显式条件）、`Healthcheck`（test/interval/timeout/retries/start_period，时间字符串经 `parse_duration` 解析，支持 `500ms`/`2s`/`1m30s`）；`Config::levels()` 拓扑分层（DFS 环检测 + 最长依赖链深度），`required_condition()` 按依赖方求最严条件（healthy > completed > started）
  - runner: `check_healthy_once`（单次 test，std 同步，含单次 timeout）；`wait_healthy`（async 预算 = start_period + interval×retries，内置 shutdown）；`run_service` 增加闸门（`Notify`）、依赖就绪表（`Arc<Mutex<HashMap<name,bool>>>`）与首轮就绪上报（started 立即 / healthy 在 select 中并行轮询 / completed 按退出码）
  - main: 前台按波次触发闸门（settle 事件收齐推进下一波），失败传播（依赖 failed → 依赖方跳过并标记 failed）；后台 `up -d` 同步波次 + 同步就绪等待后写 state
  - 语义注记：重启不重查依赖（文档注明）；healthy 与 completed 被同时要求时以 healthy 优先（边缘，文档注明）
- **Validation**: 单元（untagged 解析/环/缺依赖/分层/parse_duration/check_healthy_once）+ 集成（启动顺序断言、健康超时失败传播、completed 条件）

### Stage 8: 变量插值（.env / env_file）
- **Files modified**: `src/config.rs`、`src/runner.rs`
- **Specific logic**:
  - `parse_env_file`: 每行 `KEY=VALUE`（`#` 注释、空行、无值 KEY、首尾引号剥离）
  - `interpolate`: 状态机扫描 `${VAR}`/`${VAR:-def}`/`${VAR-def}` 与 `$$` 转义；**裸 `$VAR` 保留给运行时 shell 展开**（避免吞掉命令中的 `$i`/`$HOME`，有意偏离 compose 的裸形式插值）；查找顺序 shell 环境 > 配置目录 `.env` > 默认值
  - `Config::load` 在 parse 后 resolve：对 command/program/args/cwd/env 值/healthcheck.test 插值，再 validate
  - `EnvFile`（untagged：字符串或数组，手写 visitor）；spawn_service/spawn_detached 在 env map 之前注入 env_file 键值（env 覆盖 env_file）
- **Validation**: 单元（parse_env_file/interpolate 各语法）+ 集成（fixture 验证 command 插值、env_file 注入、默认值、shell 环境直通）

### Stage 9: 服务定向命令（up SERVICE / restart / logs -f）
- **Files modified**: `src/config.rs`、`src/main.rs`、`src/session.rs`、`tests/background.rs`、`testdata/targeted/.cue.json`
- **Specific logic**:
  - `Config::selected_services()` 校验显式名称，并递归闭包其 `depends_on`；前台/后台 `up [SERVICE...]` 过滤既有拓扑波次，只启动选集。
  - `restart [SERVICE...]` 只操作已记录的后台会话服务：先 SIGTERM、按 `--stop-timeout` 等待、必要时 SIGKILL，再以当前配置追加到原日志并持久化新 pid；无名称时覆盖会话中的全部服务。
  - 全局配置文件选项改为 `--file`，释放 `logs -f` 作为 `--follow` 短选项。
- **Validation**: 单元覆盖服务选择/未知服务/选择内最严依赖条件；集成覆盖定向前后台启动、依赖闭包、指定与全量 restart、未知 restart 不影响运行服务、以及 `logs -f SERVICE`。

## Testing Strategy
- Happy path: 多服务交错日志、前缀与着色、自然退出码汇总
- Error path: 配置缺失/坏 JSON/服务缺 command/目录不存在 → 明确报错；spawn 失败不阻塞其他服务
- 部分行冲刷：无换行结尾的输出在退出时打印
- 重启策略：always 崩溃后自动拉起（单元级验证）
- 停机：SIGINT 优雅（SIGTERM → 超时 → SIGKILL）、二次 Ctrl+C 强制
- Regression scope: 无既有功能

## Risks & Mitigation
- **子进程 fork 孙进程**：进程组整树信号解决，但孙进程若自行 `setsid` 会脱离 —— 罕见，文档注明
- **重启热循环**：固定 1s 间隔；`always` 由配置显式声明，用户自担
- **孤儿进程**：Unix 保证进程组 SIGKILL 送达，Windows 使用 `taskkill /F /T`；两者对自行脱离控制域的后代均无能为力。
- **Windows**：通过 `CREATE_NEW_PROCESS_GROUP` + Ctrl+Break 实现优雅停止，`taskkill /F /T` 兜底树终止；若进程自行脱离父子树，与 Unix `setsid` 一样不受管控。
- **Rollback plan**: 无既有代码，删除即可

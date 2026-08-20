# Cue

一键拉起项目全部开发服务的命令行工具。在项目根目录放一个 `.cue.json`，一个终端、一条命令启动前端/后端/worker 等所有服务，日志输出对齐 docker compose（彩色前缀、交错行流、优雅停机）。

## 特性

- **自动发现**：从当前目录向上逐级查找最近的 `.cue.json`，任意子目录运行即可
- **compose 风格日志**：每服务独立彩色前缀（`frontend  | ...`）、stdout/stderr 合并行流、无换行残留输出退出时冲刷
- **依赖编排**：`depends_on` + `healthcheck`，按最长依赖链分层启动，依赖未就绪下游不启动
- **后台模式**：`up -d` 脱离终端运行，`ps` / `restart` / `logs` / `down` 随时管理
- **服务定向管理**：`up [SERVICE...]` 自动包含传递依赖；`restart [SERVICE...]` 重启后台会话中的指定服务；`logs -f [SERVICE...]` 实时跟随指定服务
- **跨平台生命周期**：macOS/Linux 用进程组 SIGTERM → SIGKILL；Windows 用 Ctrl+Break → `taskkill /F /T`，前后台服务均可管理
- **变量插值**：`${VAR}` / `${VAR:-默认}`，来源 shell 环境 + 项目 `.env`；`env_file` 注入进程环境
- **重启策略**：`no` / `always` / `on-failure`（间隔 1s 防热循环）

## 安装

### 一键安装（Linux x86_64 / macOS Apple Silicon）

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/clouisle/Cue/main/scripts/install.sh | sh
```

脚本下载最新正式 Release、校验 `SHA256SUMS` 后安装到 `~/.local/bin/cue`。使用其他目录时设置 `CUE_INSTALL_DIR`：

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/clouisle/Cue/main/scripts/install.sh | CUE_INSTALL_DIR="$HOME/bin" sh
```

### 从源码安装

```bash
cargo build --release
cp target/release/cue ~/.cargo/bin/
```

### 预编译发行版

每个 Git tag 会自动创建同名 GitHub Release，包含以下已打包二进制：

- Linux x86_64：`cue-x86_64-unknown-linux-gnu.tar.gz`
- macOS Apple Silicon：`cue-aarch64-apple-darwin.tar.gz`
- Windows x86_64：`cue-x86_64-pc-windows-msvc.zip`

Windows 或需要手动安装时，从 [Releases](https://github.com/clouisle/Cue/releases) 下载对应归档，解压后将 `cue`（Windows 为 `cue.exe`）放入 `PATH`。每个发行版随附 `SHA256SUMS`，可在安装前校验下载内容。

## 快速开始

```bash
cd 你的项目
cue up                      # 读取 .cue.json，准备就绪后一键拉起全部服务
# Ctrl+C 一次优雅停止全部；再按一次强制
```

## 配置 `.cue.json`

完整示例（字段均可选，除 `command`/`program` 二选一）：

```json
{
  "services": {
    "db": {
      "command": "pg_ctl start",
      "healthcheck": {
        "test": "pg_isready -h localhost -p 5432",
        "interval": "2s",
        "retries": 30
      }
    },
    "backend": {
      "command": "cargo run",
      "cwd": "server",
      "env_file": "server/.env",
      "env": { "DATABASE_URL": "postgres://localhost:${PORT}" },
      "restart": "on-failure",
      "depends_on": { "db": { "condition": "service_healthy" } }
    },
    "frontend": {
      "command": "bun run dev",
      "cwd": "web",
      "depends_on": ["backend"]
    },
    "worker": {
      "program": "node",
      "args": ["worker.js"],
      "restart": "always"
    }
  }
}
```

### 字段

| 字段 | 说明 |
|---|---|
| `command` | 整条 shell 命令（macOS/Linux 为 `sh -c`；Windows 为 `cmd /C`）。共享配置优先使用 `program` + `args` |
| `program` + `args` | 直接 exec，不经过 shell；与 `command` 二选一，是跨平台配置的可靠形式 |
| `cwd` | 工作目录，相对配置文件所在目录；默认配置文件目录 |
| `env` | 追加覆盖到继承的环境变量 |
| `env_file` | KEY=VALUE 文件（字符串或数组），相对配置文件目录；`#` 注释、空行、无值 KEY、引号剥离；`env` 覆盖其同名键 |
| `restart` | `no`（默认）/ `always` / `on-failure`，重启间隔 1s，停机流程中不重启 |
| `depends_on` | 数组简写（等价 `service_started`）或 map：`{"db": {"condition": "service_started" \| "service_healthy" \| "service_completed_successfully"}}` |
| `healthcheck` | `test` 命令退出 0 = 健康；`interval`（默认 2s）/ `timeout`（默认 5s）/ `retries`（默认 30）/ `start_period`（默认 0s）；就绪预算 = start_period + interval × retries |

### 变量插值

- 语法：`${VAR}`、`${VAR:-默认}`（空值也走默认）、`${VAR-默认}`（仅未定义走默认）、`$$` 转义字面 `$`
- 应用于：`command` / `program` / `args` / `cwd` / `env` 值 / `healthcheck.test`
- 查找顺序：shell 环境 > 配置文件目录 `.env` > 默认值
- **裸 `$VAR` 原样保留**，由运行时 shell 展开（命令中的 `$i`、`$HOME` 不被吞；与 compose 的裸形式插值有意不同）
- `env_file` 文件不参与配置插值（仅注入进程环境）

## 命令

```
cue [OPTIONS] [COMMAND]

Commands:
  up [--detach] [SERVICE]...  Start selected services and their dependencies (default: all)
  restart [SERVICE]...        Restart background session services (default: all)
  ps                          List background session services and status
  logs                        Print background logs [-f|--follow] [--tail N] [SERVICE]...
  down                        Stop background session (graceful stop -> timeout -> force)
  config                      Print the resolved configuration as JSON
  validate                    Validate the configuration only

Options:
      --file <FILE>          指定配置文件（默认向上发现 `.cue.json`）
      --no-color             关闭彩色输出
      --stop-timeout <SECS>  优雅停机等待秒数，0 = 立即强制终止 [default: 10]
```

### 服务选择与后台管理

- `up [SERVICE...]` 和 `up -d [SERVICE...]` 只启动指定服务及其传递 `depends_on` 依赖；未选中的无关服务不会启动。未知服务名会在启动前报错。
- `restart [SERVICE...]` 只针对现有后台会话中的服务；省略名称则重启会话内全部服务。每个目标先按 `--stop-timeout` 优雅停止，必要时 SIGKILL，再以当前配置和原日志文件启动；已退出的会话服务直接启动。
- `logs [SERVICE...]` 输出所选服务的历史日志；`logs -f [SERVICE...]`（`--follow` 同义）仅跟随所选服务。`-f` 现在保留给日志跟随，配置路径使用长选项 `--file`。

### 行为细节

- **退出码**：任一服务以非零码退出 → 1（自然退出或停机后汇总）；停机时被信号终止的服务不计失败
- **前后台互斥**：后台会话运行中时，前台 `up` 与 `up -d` 拒绝启动；`restart` 仅操作已有后台会话
- **会话状态**：`$XDG_CACHE_HOME/cue/<配置路径hash>/`（fallback `~/.cache`），含 `state.json` 与每服务日志文件；`down` 只清状态，日志保留
- **依赖失败传播**：依赖启动失败（spawn 错误/健康超时/退出非零）→ 依赖方跳过启动，会话退出码 1
- **颜色开关**：TTY + 无 `NO_COLOR` + 无 `--no-color`

## 平台支持

- **macOS / Linux**：完整支持前台与后台生命周期。每个服务位于独立进程组，优雅停止发送 `SIGTERM`，超时发送 `SIGKILL`。
- **Windows**：完整支持 `up`、`up -d`、`ps`、`logs -f`、`restart` 与 `down`。每个服务位于独立控制台进程组；优雅停止发送 Ctrl+Break，超时或 `--stop-timeout 0` 使用 `taskkill /F /T` 终止整棵进程树。后台状态保存进程创建时间，拒绝把 PID 复用误判为受管服务。
- `command` 的 shell 语法不可跨系统共享；需要共享 `.cue.json` 时使用 `program` + `args`。自行脱离进程树的后代不受 Cue 管控，语义等同 Unix 子进程自行 `setsid`。

## 与 docker compose 的差异

- 面向**本机进程**而非容器：无镜像/网络/卷概念
- 健康检查默认值偏 dev（interval 2s / retries 30），compose 为 30s / 3
- 裸 `$VAR` 不插值（保留给运行时 shell）
- 多个依赖方对同一服务要求不同条件时取最严：healthy > completed > started（healthy 与 completed 同被要求时以 healthy 优先）
- 重启不重查依赖、不重报就绪（依赖编排只约束首轮启动）

## 已知边界

- 后台状态依赖操作系统进程身份；Unix 平台仍可能因 PID 复用误判 running（dev 工具可接受）。Windows 通过创建时间校验避免误杀复用 PID。

## 文档

设计文档与实施计划：`docs/plan/cue.md`、`docs/plan/cue-rename.md`、`docs/plan/cue-windows-support.md`、`docs/IMPLEMENTATION_PLAN.md`

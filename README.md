# yun-dev-manage

一键拉起项目全部开发服务的命令行工具。在项目根目录放一个 `.yun-dev.json`，一个终端、一条命令启动前端/后端/worker 等所有服务，日志输出对齐 docker compose（彩色前缀、交错行流、优雅停机）。

## 特性

- **自动发现**：从当前目录向上逐级查找最近的 `.yun-dev.json`，任意子目录运行即可
- **compose 风格日志**：每服务独立彩色前缀（`frontend  | ...`）、stdout/stderr 合并行流、无换行残留输出退出时冲刷
- **依赖编排**：`depends_on` + `healthcheck`，按最长依赖链分层启动，依赖未就绪下游不启动
- **后台模式**：`up -d` 脱离终端运行，`ps` / `logs` / `down` 随时管理
- **优雅停机**：Ctrl+C 一次 SIGTERM（整进程树），超时 SIGKILL，二次强制；SIGTERM 同路径
- **变量插值**：`${VAR}` / `${VAR:-默认}`，来源 shell 环境 + 项目 `.env`；`env_file` 注入进程环境
- **重启策略**：`no` / `always` / `on-failure`（间隔 1s 防热循环）

## 安装

```bash
cargo build --release
cp target/release/yun-dev-manage ~/.cargo/bin/
```

## 快速开始

```bash
cd 你的项目
yun-dev-manage              # 读取 .yun-dev.json，拉起全部服务
# Ctrl+C 一次优雅停止全部；再按一次强制
```

## 配置 `.yun-dev.json`

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
| `command` | 整条 shell 命令（unix `sh -c` / windows `cmd /C`） |
| `program` + `args` | 直接 exec，不经过 shell；与 `command` 二选一 |
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
yun-dev-manage [OPTIONS] [COMMAND]

Commands:
  up        Start all services and stream their logs (default command)
  up -d     Run in the background; manage with ps / logs / down
  ps        List background session services and status
  logs      Print background logs [--follow] [--tail N] [service...]
  down      Stop background session (SIGTERM -> timeout -> SIGKILL)
  config    Print the resolved configuration as JSON
  validate  Validate the configuration only

Options:
  -f, --file <FILE>          指定配置文件（默认向上发现 .yun-dev.json）
      --no-color             关闭彩色输出
      --stop-timeout <SECS>  优雅停机等待秒数，0 = 立即强制 [default: 10]
```

### 行为细节

- **退出码**：任一服务以非零码退出 → 1（自然退出或停机后汇总）；停机时被信号终止的服务不计失败
- **前后台互斥**：后台会话运行中时，前台 `up` 与 `up -d` 拒绝启动
- **会话状态**：`$XDG_CACHE_HOME/yun-dev-manage/<配置路径hash>/`（fallback `~/.cache`），含 `state.json` 与每服务日志文件；`down` 只清状态，日志保留
- **依赖失败传播**：依赖启动失败（spawn 错误/健康超时/退出非零）→ 依赖方跳过启动，会话退出码 1
- **颜色开关**：TTY + 无 `NO_COLOR` + 无 `--no-color`

## 与 docker compose 的差异

- 面向**本机进程**而非容器：无镜像/网络/卷概念
- 健康检查默认值偏 dev（interval 2s / retries 30），compose 为 30s / 3
- 裸 `$VAR` 不插值（保留给运行时 shell）
- 多个依赖方对同一服务要求不同条件时取最严：healthy > completed > started（healthy 与 completed 同被要求时以 healthy 优先）
- 重启不重查依赖、不重报就绪（依赖编排只约束首轮启动）

## 已知边界

- 进程组内孙进程若自行 `setsid` 会脱离信号管控
- 后台 pid 可能被系统复用导致 `ps` 误判 running（dev 工具可接受）
- Windows 仅保证可编译（信号退化为 taskkill 整树终止）；目标平台 macOS / Linux

## 文档

设计文档与实施计划：`docs/plan/yun-dev-manage.md`、`docs/IMPLEMENTATION_PLAN.md`

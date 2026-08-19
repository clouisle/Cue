# Implementation Plan

## yun-dev-manage：.yun-dev.json 一键多服务编排器

1. [x] 依赖与项目骨架（tokio / clap / serde）
2. [x] 配置模型与 `.yun-dev.json` 自动发现（config.rs）
3. [x] 进程编排器：spawn / 日志流 / 退出码（runner.rs）
4. [x] 优雅停机与重启策略（runner.rs）
5. [x] CLI 子命令与信号处理（main.rs）
6. [x] 集成冒烟测试（SIGINT 优雅停机）
7. [x] cargo test + clippy 全绿

## yun-dev-manage：后台模式（up -d / logs / ps / down）

1. [x] 会话状态文件与缓存目录（session.rs）
2. [x] launch_spec 抽取与后台 spawn（runner.rs）
3. [x] up -d / down / logs / ps 子命令（main.rs）
4. [x] 后台模式集成测试
5. [x] 全量验证与提交

## yun-dev-manage：依赖编排（depends_on + healthcheck）

1. [x] depends_on / healthcheck 配置模型与 levels 分层（config.rs）
2. [x] 健康检查轮询与 run_service 就绪改造（runner.rs）
3. [x] 波次启动与失败传播（main.rs，前后台）
4. [x] 集成测试与 fixture
5. [x] 全量验证与提交

## yun-dev-manage：变量插值（.env / env_file）

1. [x] 插值器 + .env 加载 + env_file 解析（config.rs）
2. [x] env_file 注入进程环境（runner.rs）
3. [x] 集成测试与 fixture
4. [x] 全量验证与提交

See docs/plan/yun-dev-manage.md

# AGENTS.md

本文件是跨 provider 的项目级单一事实源。Claude 专属细节见 [CLAUDE.md](CLAUDE.md)。

## 项目事实

- **RCoder**: 基于 ACP (Agent Client Protocol) 的 AI 驱动开发平台，Rust Cargo workspace。
- **Rust edition**: 2024（见 `Cargo.toml` `[workspace.package]`）。
- **主要 crate**: `rcoder`（主应用）、`agent_runner`（代理运行时）、`docker_manager`（容器管理）、`shared_types`（共享类型/proto）、`agent_abstraction`（ACP 连接）。
- **运行时**: 三档可切换——本地 `cargo run`（8087）、Docker Compose `make dev-up`（8090）、K8s devspace `devspace dev`（8290）。
- **gRPC**: rcoder ↔ agent_runner 使用 Tonic，proto 定义在 `crates/shared_types/proto/agent.proto`。
- **容器编排**: Docker 动态容器 + K8s STS（StatefulSet）+ PVC 保留策略。

## 验证命令

```bash
# 格式化检查
cargo fmt

# Lint
cargo clippy --workspace --all-targets

# 全量测试
cargo test --workspace

# 通过 Makefile（推荐，封装了分级测试）
make test              # 全量测试
make test-unit         # 仅单元测试
make test-integration  # 仅集成测试

# 聚焦验证单个 crate
cargo test -p rcoder
cargo test -p docker_manager

# 构建
cargo build --release --workspace
```

## 风险约束

1. **禁止 unsafe 代码** —— 项目要求内存安全。
2. **禁止模拟响应逻辑** —— 所有 AI 调用必须真实执行，不得返回 mock 数据。
3. **ACP schema 变更须谨慎** —— `shared_types` 直接嵌套 `schema::v1` 类型，升级 SDK 后务必全量编译 + 测试。
4. **agent 侧 PVC 永不删除** —— `destroy_workspace_pvc` 仅 UserApp 独立 REST 接口调用，agent 停止流程不碰 PVC。
5. **Always Response in 中文** —— 所有面向用户的响应必须使用中文。

## AI 调试路由

- **日志框架**: `tracing`，文件日志 JSON 格式按天滚动（`logs/` 目录）。
- **K8s 下 rcoder 日志写文件不写 stdout** —— `kubectl logs` 几乎为空，须 `kubectl exec ... grep /app/logs/rcoder.$(date +%Y-%m-%d)` 查询。
- **agent_runner Pod 日志在 stdout** —— `kubectl logs` 可直接查看。
- **可观测性栈**: OTLP 分布式追踪 + Prometheus 指标（`/metrics`）+ Pyroscope 持续剖析。
- 架构和调试详细指引见 [CLAUDE.md](CLAUDE.md)。

# 贡献指南

感谢你对 RCoder 的关注！本文说明参与贡献的方式与代码要求。

## 开发环境

- Rust 1.85+（2024 Edition）
- 本地 Docker（Docker Compose 开发模式），可选 K8s（OrbStack 等）
- 测试运行器：`cargo-nextest`（`cargo install cargo-nextest --locked`）

本地开发推荐 Docker Compose 模式：`make dev-build` + `make dev-up` 启动，日常改码后 `make dev-hot` 秒级热编译。详见 [README 快速开始](README.md#-快速开始)。

## 构建与测试

```bash
# 格式化与 Lint（提交前必须通过）
cargo fmt --all
cargo clippy --workspace --all-features --tests

# workspace 全量测试（nextest，收集全部失败）
make test

# 聚焦单个 crate
make test NEXTEST_ARGS='-p app_manager'

# 默认 feature 回归 / 独立 app-cli / 文档测试
make test-default
make test-app-cli
make test-doc

# 本地 Docker Compose 核心业务集成回归
make test-e2e
```

注意：

- `crates/app-cli` 是**独立 Cargo workspace**（有自己的 Cargo.lock），根 workspace 的测试不覆盖它，改动它需单独执行 `make test-app-cli`。
- `make test` 开启全部 features；涉及 K8s 的改动还需验证 `kubernetes` feature 与默认 Docker 模式两条路径。
- 不要让多个 Cargo 任务共用同一 `CARGO_TARGET_DIR` 并发运行。

## 代码要求

### Rust 安全红线

- 生产代码**禁止** `unsafe`、`unwrap()`、`expect()`（测试代码可使用）
- workspace lint 禁止 `unsafe_code`、`await_holding_lock` 等，clippy 保持零告警

### 并发与锁

- DashMap 有锁：优先使用 entry API，guard 及时释放；禁止持锁跨 `await` 或嵌套访问导致死锁
- 单线程场景不需要 DashMap

### 错误处理

- 遵循 **Fail Fast**：尽早校验输入与配置，错误主动传播，不吞错后返回成功
- 错误处用 `context()` 补充操作、对象和阶段信息

### HTTP 接口

- 使用 utoipa 描述请求、响应与错误，并核对文档注册入口
- 字段、默认值和兼容行为变化时同步 OpenAPI 与测试

### 兼容性

- 兼容行为必须有依据，不擅自增加旧字段回退、忽略有效输入或静默切换执行路径
- 暂不支持的能力明确拒绝，不虚报支持、不返回假成功

## 提交与 PR

1. Fork 仓库，从 `main` 创建特性分支
2. 提交遵循 Conventional Commits（`feat:` / `fix:` / `docs:` / `refactor:` …），描述用中文
3. Push 分支并开启 Pull Request，说明修改了什么、为什么、实际验证命令与结果
4. CI（fmt / clippy / cargo-audit / cargo-deny）通过后等待 review

## 测试有效性

- 修复逻辑缺陷时，优先补能在修复前暴露错误的回归测试
- 状态机、并发、取消、恢复类逻辑要覆盖真实调用链，不能只测 helper 或内存标记
- 不删失败断言、不降低校验、不以 skip 制造通过

## 行为准则

保持专业与友善。安全漏洞请勿开公开 issue 报告（参见 [SECURITY](SECURITY.md)）。

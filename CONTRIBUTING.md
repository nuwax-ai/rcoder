# 贡献指南

感谢你对 RCoder 的关注！本文说明参与贡献的方式与代码要求。

## 开发环境

- Rust stable（2024 Edition）
- 本地 Docker（Docker Compose 开发模式），可选 K8s（OrbStack 等）
- 测试运行器：`cargo-nextest`（`cargo install cargo-nextest --locked`）

项目开发与 CI 跟随 Rust stable。请使用仓库中的 Cargo.lock，并在验证记录中记录 `rustc --version`、`cargo --version`。项目目前未声明经过验证的 MSRV；Edition 2024 的语言最低版本不能代表整个项目及依赖的最低可编译版本。

本地开发推荐 Docker Compose：`make dev-build` + `make dev-up` 启动，日常改码后用 `make dev-hot` 增量编译并重启。详见 [README 快速开始](README.md#quick-start)及 [Docker 指南](docs/deployment/docker.md)。模型、Agent 与存储准备见[配置指南](docs/configuration.md)。

## 构建与测试

```bash
# 格式检查与默认 feature Lint
cargo fmt --all -- --check
cargo clippy --workspace --all-targets

# 完整 feature 组合
cargo clippy --workspace --all-targets --all-features

# workspace 全量测试（nextest，收集全部失败）
make test

# 聚焦单个 crate
make test NEXTEST_ARGS='-p app_manager'

# 默认 feature 回归 / 独立 app-cli / 文档测试
make test-default
make test-app-cli
make test-doc

# workspace + app-cli + 文档测试，串行收集失败；不包含部署 E2E
make test-all

# 本地 Docker Compose 核心业务集成回归
make test-e2e
```

注意：

- `crates/app-cli` 是**独立 Cargo 项目**（有自己的 Cargo.lock），根 workspace 的 fmt、Clippy、测试不覆盖它；修改它或影响它的共享依赖时，额外运行下方检查。
- `make test` 开启全部 features；涉及 K8s 的改动还需验证 `kubernetes` feature 与默认 Docker 模式两条路径。
- 不要让多个 Cargo 任务共用同一 `CARGO_TARGET_DIR` 并发运行。
- `NEXTEST_ARGS` 的 crate 筛选用于单个项目入口，不用于跨项目的 `make test-all`；`make test-doc` 不接收 nextest 参数。

```bash
cargo fmt --manifest-path crates/app-cli/Cargo.toml -- --check
cargo clippy --manifest-path crates/app-cli/Cargo.toml --all-targets
make test-app-cli
```

需要格式化时使用 `cargo fmt --all`，app-cli 单独使用 `cargo fmt --manifest-path crates/app-cli/Cargo.toml`。先运行受影响 crate 的聚焦测试，再按影响范围扩大；根 workspace 全量验证需显式使用 `--workspace`。运行测试前查看 [E2E 说明](tests-e2e/tools/README.md)与对应部署指南，组件测试不能替代 Compose/K8s 的实际业务验收。

前端探测的真实模板集成测试通过 `RCODER_FRONTEND_TEMPLATE_DIR` 指定已准备的模板仓库根目录，需要 React/Vue/Next 模板的 `package.json` 和 `backend-java/pom.xml`。未配置或对应目录缺失时沿用环境门控，该结果不能算真实模板验收。配置完成后可聚焦执行：

```bash
: "${RCODER_FRONTEND_TEMPLATE_DIR:?请设置已准备的模板仓库根目录}"
cargo nextest run -p frontend-detector --test integration --no-fail-fast
```

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

开发计划、评审提示词、交接、现场问题和每轮实际验收记录放在已忽略的 `specs/`；`docs/` 只保留公开使用指南、当前架构、业务契约和可复用验证流程。公开文档不得依赖本地内部记录。

配置示例与开发脚本使用环境变量或明确占位符，不写入真实凭据、个人 IP、SSH 用户或机器路径。个人配置放在忽略的环境文件中；提交前检查差异，`.gitignore` 不会自动移除已经跟踪的内容。

1. Fork 仓库，从 `main` 创建特性分支
2. 提交遵循 Conventional Commits（`feat:` / `fix:` / `docs:` / `refactor:` …），描述用中文
3. Push 分支并开启 Pull Request，说明修改了什么、为什么、实际验证命令与结果
4. CI（fmt / Clippy / nextest / cargo-audit / cargo-deny）通过后等待 review

## 测试有效性

- 修复逻辑缺陷时，优先补能在修复前暴露错误的回归测试
- 状态机、并发、取消、恢复类逻辑要覆盖真实调用链，不能只测 helper 或内存标记
- 不删失败断言、不降低校验、不以 skip 制造通过

## 行为准则

保持专业与友善。安全漏洞请勿开公开 issue 报告（参见 [SECURITY](SECURITY.md)）。

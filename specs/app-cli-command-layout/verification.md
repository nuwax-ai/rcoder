# app-cli 子命令参数调整（2026-09-18）

## 目标与边界

按用户决定采用不兼容的命令结构：`app-cli <子命令> <参数>`，保留 `serve`，不增加 `server` 别名。审查与实施基线为 `6aa01c1e`；该提交刚引入的 clap global 参数支持被本次调整取代。

```bash
app-cli serve --workspace /path/to/project --log-dir /path/to/logs
app-cli build --workspace /path/to/project --dev
app-cli gen-lock --workspace /path/to/project
app-cli run --workspace /path/to/project --log-dir /path/to/logs
app-cli run-service RELEASE_ID SERVICE_ID --log-dir /path/to/logs
```

- 所有命令必须显式指定。顶层 `--workspace`、`--gen-lock` 和无子命令执行返回解析错误（exit 2）。
- workspace 参数由需要它的子命令复用；管理 API、Pingap、日志参数属于运行命令，`--attach` 只属于 `serve`。
- 保留已有参数环境变量及 CLI 优先级；删除通过 `APP_CLI_GEN_LOCK` 选择动作的方式。
- `run` 为原直接前台编排入口的显式名称，保留 file-server 平台开发链路行为；这不是运行态所有权迁移，也不新增“后来启动者接管”语义。
- 将解析结构 `CliArgs` 与内核配置 `RuntimeArgs` 分离；内部运行函数改接收后者。
- `gen-lock` / `build` 在日志和运行服务初始化前分派，宿主机工具用法不依赖容器日志目录。

## 配套调整

1. file-server 的直接启动参数改为 `run --workspace ...`，受控进程测试检查参数前缀。
2. app-cli 二进制启动、重启、进程树、Windows 测试全部迁移到对应子命令。
3. npm 发布工作流 pair-gate 改用 `gen-lock --workspace`；E2E 热部署 builtin 入口改用 `serve --workspace`。
4. 更新 npm README、UserApp CLI 使用说明及构建输出提示。
5. companion 仓库 build-agent-docker 的 `rcoder-app-runtime` 已使用正确命令，仅修正“global / 两种顺序兼容”的过期注释。

这是一项破坏性 CLI 变更。file-server 与 app-cli 必须配套构建、发布和升级，不能把新 file-server 与旧 app-cli 二进制混用。此轮不执行版本发布、提交、推送、镜像构建或集群部署。

## 验证

新增反例在改动前运行：

```bash
cargo nextest run --manifest-path crates/app-cli/Cargo.toml --test cli_contract --no-fail-fast
```

基线退出码 100，4/4 失败：缺少新命令、旧参数仍接受、无关参数仍接受、无命令未在解析阶段拒绝。改动后原 4 例全部通过。

新增真实 `gen-lock` 临时工作区测试，覆盖带空格路径、CLI 覆盖环境变量、仅环境变量指定 workspace、实际生成 lock，以及不初始化运行日志/监听器。

### 当前结果

| 验证 | 结果 |
|---|---|
| app-cli `--test cli_contract` 改动后 | exit 0；4/4，通过后新增真实 gen-lock 用例 |
| app-cli 聚焦 `test(per_operation_pg_credential_reaches_service_env) | binary(cli_contract) | test(config::tests)` | exit 0；8/8（5 个 CLI 集成、2 个参数解析、1 个凭据测试） |
| app-cli 全量 `--no-fail-fast --all-features` | exit 100；212 通过，1 个断言失败，1 个挂起测试被 SIGINT 终止 |
| 基线 `6aa01c1e` 对照：凭据测试 + `binary(tree_lifecycle)` | exit 100；凭据通过、startup failure 测试失败、同一进程树测试挂起被 SIGINT 终止 |
| 排除已复现挂起项后再次 app-cli 回归 | exit 100；211 通过，2 失败，1 排除；不算完整通过 |
| file-server `test(service::dev_server::start::tests)` 默认 / `--all-features` | 均 exit 0，各 1/1；验证真实 spawn 参数与凭据传递 |
| app-cli `cargo clippy --manifest-path crates/app-cli/Cargo.toml --all-targets` | exit 0；仍有基线已存在的 `endpoint_matches_identity`、`read_token` dead_code 警告 |
| app-cli fmt check、修改的 file-server 文件 rustfmt check、两个仓库 diff check | exit 0 |
| hot_contract.py 的 Python AST 解析、companion 启动脚本 `sh -n` | exit 0 |

### 待处理的测试问题（本轮未修改业务逻辑或降低断言）

1. `supervised_stop_kills_term_ignoring_grandchild_and_releases_ports`：当前版本与基线均超过 60 秒未结束，分别人工 SIGINT 终止。`crates/app-cli/tests/tree_lifecycle.rs:218` 的 `wait_log_marker` 在阻塞 `lines()` 返回后才检查 deadline；缺少新日志时这个预算无法唤醒读取。后续应为管道读取设置真正的外部超时，并确保失败路径收束子进程树。
2. `startup_failure_cleanup_converges_service_tree`：首轮当前版本通过，基线和末轮当前版本均出现 `service root ... never became occupied`。基线已复现该失败，但具体环境/时序原因仍需定位；不能通过删断言或延长等待直接认定修复。
3. `per_operation_pg_credential_reaches_service_env`：当前全量两轮断言失败，当前聚焦和基线聚焦通过；基线未复现同样失败，标为待归因。`crates/app-cli/src/supervisor.rs:1085` 读到非空文件即断言，可能读取 `env | sort` 写入中间态。后续应以写端完成/原子发布文件建立同步，再核对生产环境覆盖逻辑；不能只轮询到期望凭据出现就算验证完成。

因此本轮结论是：CLI 契约与调用方聚焦验证通过，完整回归未全绿。上述失败不作为发布通过依据。

### 实际命令与证据

```bash
cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast --all-features
cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast -E 'test(per_operation_pg_credential_reaches_service_env) | binary(cli_contract) | test(config::tests)'
cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast --all-features -E 'not test(supervised_stop_kills_term_ignoring_grandchild_and_releases_ports)'
cargo nextest run -p file-server --no-fail-fast -E 'test(service::dev_server::start::tests)'
cargo nextest run -p file-server --all-features --no-fail-fast -E 'test(service::dev_server::start::tests)'
cargo clippy --manifest-path crates/app-cli/Cargo.toml --all-targets
cargo fmt --manifest-path crates/app-cli/Cargo.toml -- --check
rustfmt --edition 2024 --check crates/file-server/src/service/dev_server/start.rs
```

基线在临时 detached worktree 对同样测试执行；复用 app-cli target 的编译顺序串行，完成后移除临时 worktree。基线 nextest 使用 `-E 'test(per_operation_pg_credential_reaches_service_env) | binary(tree_lifecycle)'`，总退出码 100。原始日志保存在本机 `/tmp/rcoder-cli-*.log`，不纳入提交；日志可能包含测试子进程继承的环境信息，交付仅保留上述脱敏结论。

## 未覆盖

未执行 Windows/Linux 原生机器、Compose、remote K8s 或 npm 发布。Windows 测试源码与 CI 命令已迁移，不能据此声明三平台实机通过。历史审查报告保留其原基线语法，不改写历史证据。

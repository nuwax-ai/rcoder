# 随仓复核反例

这些工具帮助另一台电脑独立核验 [审查记录](../README.md)。它们不依赖本机 `specs/`、日志或历史二进制；不属于已通过的正式产品测试。某些脚本退出 0 表示“缺陷确实存在”，修复后其历史缺陷断言应失败，应把正确行为纳入正式回归测试。

提交前只对移植后的 Python／Rust 工具做语法、参数及路径检查，没有重新启动实例或执行 Cargo；DOM 工具按 Node 22 实际执行过。原审查的行为证据对应本地原反例版本，不能自动移作移植版本的运行证明。下一位复核者必须在自己的源码、工具链和隔离环境重新执行。

先读取每个脚本。目录与进程必须自建隔离，不能使用生产应用路径。Cargo 串行，所有构建、缓存、临时文件和证据放用户指定工作盘；复用既有工具链／缓存配置，不修改全局设置。下面从仓库根执行，`REVIEW_WORK_ROOT` 替换为工作盘上本轮拥有的目录。

```bash
export REVIEW_WORK_ROOT=/path/on/work-disk/rcoder-review
mkdir -p "$REVIEW_WORK_ROOT"
export CARGO_TARGET_DIR="$REVIEW_WORK_ROOT/cargo-target"
export TMPDIR="$REVIEW_WORK_ROOT/tmp"
mkdir -p "$TMPDIR"
# CARGO_HOME/RUSTUP_HOME/npm 缓存使用已有工作盘配置；Node 为 22，Rust 为 stable。
```

## R1：原客户端事件与真实 owner／Stop

先串行构建当前源码的独立 app-cli，不能拿旧安装包代替：

```bash
cargo build --manifest-path crates/app-cli/Cargo.toml --all-features
export REVIEW_APP_CLI="$CARGO_TARGET_DIR/debug/app-cli"
"$REVIEW_APP_CLI" --version
```

Python 3.11+、POSIX `fcntl` 环境：

```bash
python3 docs/development/reviews/2026-10-08-userapp-startup/fixtures/event_protocol.py \
  "$REVIEW_APP_CLI" --destination "$REVIEW_WORK_ROOT/event-protocol-01"
python3 docs/development/reviews/2026-10-08-userapp-startup/fixtures/event_stop_before_commit.py \
  "$REVIEW_APP_CLI" --destination "$REVIEW_WORK_ROOT/event-stop-01"
```

destination 必须是新目录。前者是实际 CLI＋真实内核 owner 锁＋受控 HTTP owner，检查跨轮 Done 输出；后者使用实际 owner、业务 HTTP 和 Stop，但 Pingap 是受控进程并显式跳过确认。它们不是完整 Pingap／容器 E2E。后者只通过捕获的实例与代次关闭自建 owner并确认锁释放，不按裸 PID、端口或进程名清理未知服务。

阅读 stdout、原操作与 Stop 收据，检查第一个成功 Done 时原操作是否已成功提交，以及终局是否重复／矛盾。修复后的断言须围绕原操作权威终态，不能只断言“拿到了 Done”。

## R2/R3：真实 file-server HTTP 移交

```bash
python3 docs/development/reviews/2026-10-08-userapp-startup/fixtures/owner_http.py \
  --work-dir "$REVIEW_WORK_ROOT/owner-http-observe-01"
# 有依赖缓存时可加 --offline。
python3 docs/development/reviews/2026-10-08-userapp-startup/fixtures/owner_http.py \
  --work-dir "$REVIEW_WORK_ROOT/owner-http-assert-01" --assert-correct
```

该 launcher 在独立新目录装配小型 Cargo fixture，依赖实际 file-server crate和当前锁定依赖，未修改生产 Cargo.toml；真实 Router → manager → shell 退出码。owner 是只读身份 fixture，直到 spawn 后才绑定；业务 9080 是自建 503 服务，被占用即拒绝，不替换已有服务。

默认观察模式记录 exit17／exit0 的实际 HTTP、监督退出及重复 Start 结果，基线可 exit0但表现错误；`--assert-correct` 在基线应失败。后者只要求非零启动不被报成功、正常移交后重复 Start 不被死 run 的历史 guard 阻挡。只读 owner 没有凭据／操作 API，因此修复后可以报告具体能力错误，不要求它伪报业务成功。完整 owner 复用成功仍需正式真实 app-cli 测试。

## R6：内置页面实际 JS 与 locale

Node 22，无 npm 安装或浏览器依赖：

```bash
node docs/development/reviews/2026-10-08-userapp-startup/fixtures/proxy_page_dom.cjs
# 可选输出到尚不存在的证据文件：
node docs/development/reviews/2026-10-08-userapp-startup/fixtures/proxy_page_dom.cjs \
  --output "$REVIEW_WORK_ROOT/proxy-dom-before.json"
```

读取当前内置 HTML 中实际脚本和三语言 title，使用受控 DOM 执行。基线 exit0 证明语言引起状态失配，不能视为页面通过；修复后须扩充为正确 cause／状态／固定译文的正式测试，并在真实浏览器检查。

修复版组件回归另存为 `proxy_page_regression.cjs`，不改写历史反例：

```bash
node docs/development/reviews/2026-10-08-userapp-startup/fixtures/proxy_page_regression.cjs
```

读取实际内置 HTML／脚本和 locale，用受控 DOM 验证三语言 × 九档 cause
的节点状态、译文、加载／停止图形及复制提示；exit 0 表示这些组件断言通过。
它补充 Rust 的模板注入测试，不等于真实浏览器、复制权限或容器 E2E。

## R4/R5/R7/R8

- R4：为实际 handler／manager 注入慢运行时、慢存储、慢 owner，断言共同 deadline；旧查询 late-success不能在预算后报告 Ready。不要只测单独超时 helper。
- R5：通过真实前置查询超时产生 WakeFailure，断言没有启动证据时不显示 Starting。
- R7：扩展现有 `success_done_after_observed_exit_still_succeeds`，检查原 task 事件中的 `event:"log"` 摘要及 service_id。
- R8：运行 `cargo clippy -p file-server --all-targets -- -D warnings`，修正三项告警后再运行受影响默认／all features门禁。

将有效反例纳入正式测试。随仓脚本、历史 stdout 或静态审查不能替代本轮默认／全 feature、真实容器、K8s 和实际存储的验证结果。

# 2026-10-08 UserApp 启动与代理改动审查

本记录用于跨电脑复核与修复交接。审查结论是待修复项，不代表已实现、已部署或已发布；复核者应先读取当前源码，再用反例核验，不能直接采信叙述。

审查分支为 `feature-userapp`，范围为 `ace54df66cadb1384b60e324eb6a15fdd632f726..76772b5752296658572bfddac1851c6b1520d979`：六个提交、24 个文件。审查开始及结束工作树干净，生产源码未修改。实际工具链为 Rust 1.99.0、cargo-nextest 0.9.146、Node 22.23.2；此处记录实测身份，不要求下一轮固定这些数值。Rust 使用 stable，Node 保持 22。

原始日志和完整运行收据留在审查电脑的本地内部目录，没有随仓提交；另一台电脑不需要这些文件才能复核。本文包含源码触点、观察结果、反证条件和验收要求，并提供 [复核开发提示词](CLAUDE-CODE-PROMPT.md) 与可移植的 [反例](fixtures/README.md)。下面行号对应审查基线，后续应按函数定位。

## 问题与证据等级

A 表示本轮执行过有行为断言的组件／协议／DOM 反例；B 表示实际源码调用链证据。A 不自动等于完整产品 E2E：受控组件与未覆盖部分在每条中单独说明。

| 编号 | 优先级 | 问题 | 证据 | 状态 |
|---|---|---|---|---|
| R1 | P1 | 成功 Done 早于原操作成功提交，Stop 后无法纠正 | A：真实 owner、业务 HTTP、Stop；受控 Pingap | 未修复 |
| R2 | P1 | run 非零退出被管理面存活转换为启动成功 | A：真实 file-server HTTP、manager、shell；受控 owner | 未修复 |
| R3 | P2 | 成功移交保留死 run 登记，重复 Start 被挡住 | A：同一真实 HTTP 反例 | 未修复 |
| R4 | P2 | 创建观察和 owner 移交预算未钳制内部 I/O | A：原函数边界；B：owner 探测调用链 | 未修复 |
| R5 | P2 | 查询／预检超时被宣称为正在启动 | B：真实错误产生及呈现分支 | 未修复 |
| R6 | P2 | 国际化标题破坏旧 JS 状态判断与固定文案 | A：实际脚本和 locale 的 DOM fixture | 未修复 |
| R7 | P2 | 合法的退出后成功顺序遗漏 log 成功摘要 | B：实际 consumer 与新增测试 | 未修复 |
| R8 | P2 | 严格 Clippy 被三处新增告警阻断 | A：实际 Clippy，exit 101 | 未修复 |

## R1：成功 Done 早于运行提交屏障

触点：

- `crates/app-cli/src/control/owner_dispatch.rs` 的观察循环（基线 621–636 行）。
- `crates/app-cli/src/control/dispatch_events.rs::forward_operation_events`（46–69 行）。
- `crates/app-cli/src/services/supervisor/run.rs` 的 `OrchestrationDone`、`wait_for_bridge`、`on_running`。
- `crates/app-cli/src/orchestration/server/run_loop.rs::commit_running_barrier`。
- `crates/file-server-userapp/src/service/userapp/start_events.rs::consume`（236–241 行）。

新桥在原操作非终态时就把 journal 的 `orchestration_done` 写到 stdout。该事件早于 bridge 等待和运行内核提交屏障，不能证明原操作已经 Succeeded。消费者遇到第一个空失败清单就返回成功，之后不再消费 Failed／失败 Done。

本轮真实组件反例的顺序为：

1. 客户端 stdout 出现 `orchestration_done`，`failed=[]`。
2. 经真实管理 API 读取原 Source 操作，状态仍为 **Accepted**。
3. 提交真实 Stop：Stop 为 **Succeeded**，原 Source 操作为 **Cancelled**，业务端口关闭，run client 退出 **1**。
4. stdout 又出现 Failed 和带失败清单的 Done。

该反例使用真实 CLI、owner、业务 HTTP、Stop 和捕获身份关闭；Pingap 是受控进程并显式跳过确认，因此它证明 owner／提交屏障／Stop 时序，不证明完整 Pingap 或 Docker/K8s 验收。另一个持锁 HTTP 协议反例确认：`done_forwarded` 每轮重置，成功跨轮输出 `Done(empty) → Completed → Done(empty)`，取消跨轮输出 `Done(empty) → Failed → Done(failure)`。

反证条件：第一条可消费的成功终局必然发生在同一捕获操作 Succeeded 后；Stop 在提交前受理时消费者不能成功；同一操作的成功／失败终局各路径都仅一次。仅增加跨轮 `done_seen` 只能消重，不能修正提前成功。

修复方向：把对外终局绑定原操作的权威结果，保留原服务明细与操作身份，不用最近操作代替原操作。同时核验事件获取失败、分页、EOF、退出和取消路径，不能把错误静默变为成功。

## R2：run 非零退出被 owner 存活掩盖

触点：`crates/file-server/src/service/dev_server/start/alive.rs::wait_alive` 的 ConfirmOwner 分支（125–128 行），以及 `handlers/build/dev.rs::start_dev`、`keep_alive`。

新分支只核验 owner identity，不读取原监督退出状态或原操作结果。app-cli 按产品契约在业务失败后保留管理 owner，当前 `crates/app-cli/tests/bin_startup.rs` 也实际覆盖了“客户端非零退出、管理面在线”。因此管理面存活无法证明原启动成功。

真实 `/api/build/start-dev` Router → DevServerManager → shell 反例中，子进程 **exit 17**，独立业务 fixture 返回 503，匹配 owner 仍在线，接口却返回 **HTTP 200、success:true、Development server started**。此处 owner 为受控身份 HTTP 服务，直到 spawn 后才绑定，排除了前置复用；它不是完整 app-cli 或容器 E2E。UserApp 外层事件 barrier 对部分失败另有保护，不能据此概括所有入口均误报成功。

反证条件：该组合返回原请求失败／取消／未确认结果，保留具体原因，同时管理服务继续存在。业务 `ready=false` 本身不是失败证据，不得为了修复吞错而改变既有就绪边界。

修复方向：区分“owner 已接管”和“原启动请求成功”，保留监督退出信息并读取原操作结果；不要删除管理 owner，也不能只按裸 PID 判断执行身份。

## R3：成功移交未完成登记转换

触点：`start/manifest.rs` 的 run 登记、`wait_manifest_alive` 后续返回，以及文件开头的本地 orchestrator guard。

新等待路径接受 run 正常退出后返回 Ready，但 `processes` 仍保存已经退出的 run PID、`external_owner=None`。重复 Start 在发现可复用 owner 前被 `local orchestrator is already registered` 拒绝。

同一真实 HTTP 反例的 exit 0 场景：首次 success=true；`pid_running=false`、`has_external_owner=false`；第二次 HTTP 400 且命中上述 guard，owner 仍回应。guard 是历史代码，本轮新增成功移交路径使该问题稳定可达；不能把历史 guard 本身误称新增代码。

反证条件：正常移交后再次 Start 经统一 owner 链受理／查询，不被死客户端登记挡住；Stop 与新实例竞争时旧移交不能退休或覆盖后继登记。

修复方向：按捕获 launch／实例身份原子完成登记交接，或退休原 launch 后以核验过的 owner 登记复用。补正常移交、失败移交、重复 Start 和 Stop／successor 竞争反例。

## R4：观察预算不是硬边界

`crates/http-server/src/handler/pod_handler/ensure.rs::wait_creation_observed` 的 `get_app` 与 `get_compute_control` 直接 await，15 秒检查发生在 I/O 之后；慢成功读取还直接返回 true。逐字提取原函数正文、使用受控等待的边界反例：20ms 预算、80ms runtime 读取，实际 82ms；80ms store 读取，实际 83ms。这是函数边界证据，不是 HTTP/K8s E2E。实际 `get_app` 还会读取运行时对象和存储策略，因此单轮底层读取同样未受预算约束。

`crates/file-server/src/service/dev_server/start/alive.rs::confirm_owner_handover` 的 2 秒预算仅在失败后检查，内部 `owner_client::observe_owner` 使用 3 秒 HTTP timeout；收尾探测未夹父 deadline，之后也没有父 deadline 复核。此部分为源码证据，未跑独立慢 owner 网络反例。

反证条件：慢 runtime／store／owner 请求在阶段与父 deadline 的较早者以内结束观察；超时不报告 Ready，不误归启动退出，不主动停止结果未知的原执行。

修复方向：观察、重试及 sleep 共用绝对 deadline；预算耗尽返回已受理但观察未完成，保留原 operation_id、最新已确认视图和后续查询能力。

`creation_observed` 当前定义仅为“工作负载对象存在”，旧 replicas=0 Deployment 满足此定义不单独判错；它不能证明本次 Start 已执行，也不代表业务 Ready。若需求需证明本次物理写入已观察，应新增明确身份／回执条件，不能静默改变字段语义。

## R5：前置查询超时被误判 Starting

触点：`crates/rcoder-proxy/src/error_page.rs::wake_failure_cause`（176–179 行）。

`ERR_RUNTIME_TIMEOUT` 也产生于 `proxy_http.rs` 的 `wake_runtime_probe`：该分支超时即返回，后面的 `ensure_running` 尚未调用。`app_manager/lifecycle/wake.rs` 的 `wake_preflight` 也可能尚未受理启动。新映射仅按错误码输出“正在启动”，没有启动证据。

反证条件：前置查询、身份读取及未知状态超时使用符合原原因的档位；只有捕获实例确实在启动／等待就绪时才选择 Starting。状态码、阶段、原操作和安全重试字段保持一致。

修复方向：保留完整 WakeFailure，根据结构化阶段和已捕获执行证据分类，不解析中英文消息，也不能把所有超时都当启动。

## R6：国际化与旧标题解析不兼容

新多语言 title 经 `error_page.rs` 渲染到内置页，但 `crates/rcoder-proxy/assets/userapp-error-default.html` 的脚本仍按简体标题子串识别状态。实际脚本＋实际 locale 的 DOM fixture 确认：英文 Starting／Stopped 和繁体 Starting 保持红叉“不可用”；默认无语言头的英文页同样受影响。按钮、状态、复制提示等仍固定简体中文。这不是完整浏览器验收。

反证条件：相同 cause 在三语言中显示相同的结构化状态与正确译文，文案变化不会改变分支；默认英文也正确。

修复方向：用结构化 cause 驱动 UI；一起国际化固定标签，更新新增模板变量的管理上传白名单与示例。保留 HTML 转义、状态码、HEAD／JSON／SSE 既有协议行为。

## R7：新合法成功顺序缺 log 摘要

`file-server-userapp/start_events.rs` 新增测试认可 `Exited → ServiceStartOk → Done(empty)` 成功，但 ServiceStartOk 的 log 分支仍要求 `exited.is_none()`，Done 只补失败日志。结果成功却没有启动成功 `event:"log"`；Completed 分支也不补该摘要。本条为源码证据，新增测试没有检查实际任务事件日志。

反证条件：该合法顺序在原任务中有真实 service_id 的成功摘要；失败有原原因摘要，重复事件不刷屏，具体错误不被空原因覆盖。须断言 `target.subscribe(0)` 或真实任务 SSE，不只检查 consumer 返回值。

## R8：严格 Clippy 失败

实际命令 `cargo clippy -p file-server --all-targets -- -D warnings` exit **101**：

- 新 `start/alive.rs::wait_alive` 为 8 参数，触发 `too_many_arguments`。
- 新 `start/tests.rs` 两处 `let _ = child.wait()`、`let _ = axum::serve(...).await` 触发 `let_underscore_drop`。

`.github/workflows/quality.yml` 的 workspace 默认及全 features 检查实际使用 `-D warnings`，会被阻断。修复参数组织及明确结果处理，不为过门禁整体放宽 lint，也不把测试中的真实 wait 错误静默忽略。

## 本轮验证与限制

| 检查 | 实际结果 |
|---|---|
| app-cli 聚焦 nextest，all features | exit 0；24 通过、373 因筛选未执行 |
| 四个受影响 crate，默认 features 聚焦 nextest | exit 0；165 通过、764 因筛选未执行 |
| 同一表达式，all features（含 Kubernetes） | exit 0；165 通过、764 因筛选未执行 |
| file-server 严格 Clippy | exit 101；R8 三项告警 |
| root 与独立 app-cli fmt check | 均 exit 0 |
| R1 原 CLI／owner／Stop、协议反例 | 已确认问题；不是完整容器验收 |
| R2/R3 真实 HTTP → manager → shell 反例 | 已确认问题；owner 与业务为受控 fixture |
| R4 原函数边界反例、R6 DOM fixture | 已确认问题；分别不等于 HTTP/K8s、完整浏览器验收 |

聚焦命令：

```bash
cargo nextest run --manifest-path crates/app-cli/Cargo.toml \
  --no-fail-fast --all-features -E 'test(dispatch) | binary(bin_startup)'
cargo nextest run -p file-server -p file-server-userapp -p http-server -p rcoder-proxy \
  --no-fail-fast -E 'test(dev_server) | test(start_events) | test(pod_handler) | test(error_page)'
cargo nextest run -p file-server -p file-server-userapp -p http-server -p rcoder-proxy \
  --all-features --no-fail-fast \
  -E 'test(dev_server) | test(start_events) | test(pod_handler) | test(error_page)'
```

这些现有测试通过不能反证上述新反例。没有执行完整 root／app-cli nextest、完整 Compose/K8s 容器 E2E、生产 Pingap、部署或发布验收。修复后按影响补原失败反例和组件／容器验证，逐项记录基线、命令、退出码、实际行为与未覆盖平台。

## 2026-10-08 复核与修复结果（第二轮，接手会话）

原始审查记录保持原样（上表与各条证据为第一轮基线）。本轮逐项对照当前源码复核后确认 R1–R8 全部成立，并完成修复；修复要点与正式回归测试（取代一次性 fixture 断言）：

| 编号 | 复核结论 | 修复要点 | 正式回归 |
|---|---|---|---|
| R1 | 成立 | journal `orchestration_done` 改为捕获不转发（`dispatch_events.rs`）；终局 Done 在原操作权威终态时恰好一次发出——捕获件与视图终态一致（空失败清单 ⇔ Succeeded）才保留原服务明细，否则按视图合成 | app-cli 单测×3 + `run_source_owner.rs` 真实链路×2（成功路径 done⇔权威终态绑定、恰好一次；Stop 竞争路径单一失败 done） |
| R2 | 成立 | `ExitPolicy::ConfirmOwner` 以监督退出码为直接证据：run 非零退出即启动失败（stderr 分类），owner 存活不再掩盖（早退分支与宽松收尾双出口） | `manifest_wait_reports_failure_when_run_exits_nonzero_despite_owner`（exit 17 + 匹配 owner 在线 → 失败） |
| R3 | 成立 | 移交成立后登记转换为外部 owner（`external_owner` 就位，token 空哨兵由停止路径重读）；重复 Start 走 owner 复用链 | `manifest_handover_converts_registration_for_repeat_start`（移交→登记转换→二次 Start 提交 Restart） |
| R4 | 成立 | `wait_creation_observed` 抽出 `observe_creation_with` 核：单轮 runtime/store 读取钳制到共享 deadline，三个耗尽出口均留 warn 日志；`confirm_owner_handover` 单次探测钳制剩余预算、收尾复验钳制父 deadline | `creation_budget_tests`×4（20ms 预算 vs 80ms 慢读边界、终态短路、正常路径） |
| R5 | 成立 | `wake_failure_cause` 按 `WakeFailure.stage` 结构化分类：`wake_wait`/`wake_observation`/`wake_follower_wait`（有启动证据）→ Starting；`wake_preflight`/`wake_runtime_probe`/`file_credentials_configuration` → PlatformUnavailable；不解析消息文本 | `timeout_stage_decides_starting_claim` + 映射表测试更新 |
| R6 | 成立 | 内置页脚本改按服务端注入的 `data-cause` 档位驱动（不解析翻译标题）；固定标签/aria/按钮/复制提示经 `RCODER_UI_*` 占位符按 locale 渲染；白名单与管理面文档同步 | `builtin_page_renders_cause_slug_and_localized_ui_labels`（三语言 data-cause + 标签、默认英文页无中文残留）+ 白名单用例 |
| R7 | 成立 | `ServiceStartOk` 成功摘要不再被"已观察退出"guard 剥夺（终局判据是 Done，管道字节序下退出通知先到是合法顺序） | `success_done_after_observed_exit_still_succeeds` 扩展：断言任务事件含真实 service_id 的 `event:"log"` 成功摘要 |
| R8 | 成立 | `wait_alive` 参数收拢为 `AliveWatch`（8 参 → 3 参）；两处 `let _ =` 改显式处理 | `cargo clippy -p file-server --all-targets -- -D warnings` exit 0 |

同轮完成的关联改造（2026-10-08 app 221 预上线事故的转发层加固，方案见会话记录）：dev 转发发送前 TCP 有界预检 `wait_for_dev_service`（对齐 prod `wait_for_prod_service` 语义，默认 20s 可配置），在途 Dev 操作作为失败分级证据（`ERR_OPERATION_IN_PROGRESS`），无证据的不可达分类为 `ERR_CONTAINER_ADDRESS_NOT_READY` + Retry-After + 本地化文案；发送层连接失败不再裸抛 reqwest 原文。e2e `userapp_dev_registry_self_heal_after_restart` 增加窗口不变量断言。

未覆盖项：真实浏览器下的错误页 JS 行为（DOM fixture 与旧模板耦合已失效；Rust 测试覆盖注入契约）；compose 容器级 R2/R3 复验由 e2e 既有 dev 场景回归；K8s 模式未在本轮验证。

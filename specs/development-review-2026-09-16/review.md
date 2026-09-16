# 前序开发任务复核：问题与修复验收

日期：2026-09-16。审查基线：`7b690562b0b03bc3390bb787a9662ab9b85fa2f7`。

## 1. 结论与证据边界

四个任务均有实际开发进展，但当前实现存在影响控制操作、数据升级和工作目录选择的缺陷。尤其新 runtime API 不宜据组件测试通过认定可交付。

本轮核对当前源码、提交差异、Spec/Plan/Tasks，以及本地 nuwax-file-server、kube-runtime 4.2.0 源码。以下是源码可以确定的缺陷及触发条件；**没有运行 Cargo、Compose 或集群测试，没有把其他 agent 的历史测试报告当成本轮结果，也没有宣称已在线复现**。

审查期间已有另一 agent 执行集成测试，初始未提交文件为：

- `crates/app_manager/src/utils.rs`
- `tests-e2e/tools/acceptance_steps.json`

未修改这些文件、业务代码、部署或被测环境。本轮只新增审查和交接文档。看起来局部的代理参数修复也会改变被测源码摘要，统一留待测试完成后修复。

P1：应在相应功能交付前修复。P2：契约不一致，需要修复，但影响相对局部。下述行号对应审查基线，后续先按符号重新定位。

## 2. 发现的问题

### R01 · P1 · Running 状态不消费 stop/restart，操作结果可能记到错误 ID

**证据**：`crates/app-cli/src/server.rs:975`、`:989`、`:1001` 在入队时覆盖全局 `current_runtime_operation`；`:1275` 仅 Idle 分支消费 `control_rx`。supervisord Running 等待 `:1453` 与 builtin Running 等待 `:1537` 只处理部署/退出，没有消费该控制队列。取控制消息时还丢弃了消息携带的 operation_id。`runtime_kernel.rs:423` 允许 active 期间受理 Stop。

**触发与后果**：

1. 服务正常 Running 后受理 Stop/Restart：消息进入 control 队列，但工作循环仍等待部署或退出，操作持续 Accepted。
2. A 正在启动时受理 Stop B：全局当前 ID 被改为 B；A 到 Running 调用 `finish_current_runtime_operation(Succeeded)` 时可能完成 B，而停止实际尚未执行。

**修复**：每个执行动作携带不可变操作身份；受理不能覆盖正在执行的身份。两种引擎都在启动、运行阶段接入停止/取消屏障；旧启动提交前重新检查 revision。旧部署 API 与新 runtime API 共用受理和执行约束，不能有两套互不认识的 busy 状态。

**验收**：两引擎 Running→Stop、Running→Restart；慢启动 A→Stop B；重复 Stop；旧部署接口与新接口并发。断言真实服务状态、A/B 各自终态、revision、事件归属，不能只断言 HTTP 202。

### R02 · P1 · 状态目录位于会被热部署整体替换的 workspace 内

**证据**：`runtime_kernel.rs:58` 使用 `workspace.join(".app-cli-state")`；`server.rs:945` 传入 `args.workspace`。`deploy.rs:286` 的 activate 将整个 workspace 改名为 `.previous`，再用 staging 替代；下一次激活还会清除 `.previous`。

**后果**：本轮部署对应的操作记录、身份、desired state 随旧目录移动；部署完成时持久化访问原路径，已经指向新制品目录。历史可能查询不到，后续初始化可能重建身份/修订号，下一次部署还可能删除旧记录。

**修复**：使用显式稳定状态根，位于源码根和任何可替换目录之外；锁、身份、操作日志、期望状态使用同一所有权域。不要仅换成另一种 `parent()` 猜测。明确已有状态如何迁移及冲突时如何阻止写入。

**验收**：连续两次部署和控制进程重启，身份/历史/revision 保持；源码与 `.run` 入口竞争同一锁；目录切换中断不能新建第二个权威状态域。

### R03 · P1 · Stopped 未参与启动恢复，取消与 profile 能力没有完整接线

**证据**：`server.rs:783` 先执行 `initialize_startup`，`:805` 后装配新内核；新 store 的 `load_desired` 没有用于 server 启动决策。`runtime_kernel.rs:581` 的 `request_cancel` 仅检查 active ID 并返回 true，没有记录取消或通知执行器。`:495` 的派发仅特殊处理 Deploy+Artifact::Url 和 Stop，其余均变成 OrchestrateSource；身份却在 `:103` 宣告 source/artifact profile 能力。

**后果**：持久化 Stopped 不足以防止重启恢复业务；取消请求被接受却不生效；部分 artifact/profile 请求可能按当前 workspace 编排，忽略请求输入。

**修复**：恢复必须先读可信 desired/recovery 状态再决定是否启动；Stopped 保持 Idle。取消应连接操作级执行信号及有界清理，清理未知保持 RecoveryRequired。按协议逐一实现 profile 输入、devrun/devbuild 与制品语义；未支持组合明确拒绝，不得宣告支持后静默走通用分支。

**验收**：Stop 后进程/Pod 重启不复活；构建/启动/切换各阶段取消；清理失败保持保护；请求指定的 profile 真正影响执行；不支持输入立即报结构化错误。

### R04 · P1 · 持久化失败与损坏记录没有可靠阻止后续写操作

**证据**：`runtime_kernel.rs:191` 使用 `entries.flatten()`，读取失败或 JSON 解码失败均 continue；`:316` 恢复结果为空时清除 recovery protection。`:465` 先写 Accepted 操作，再写 desired；后者失败直接返回错误，未建立 active/recovery 保护。

**触发与后果**：

- 唯一未终态操作记录损坏/不可读：日志称 fail closed，实际恢复列表为空，新写可能被放行。
- Accepted 落盘后 desired 写失败：同 ID 重试返回永远未派发的 Accepted；其他 ID 仍可能被受理。

**修复**：扫描/读取/解码失败必须阻止相关运行态写入；操作受理、意图、revision 建立可恢复的一致性协议（事务或明确提交记录与恢复规则）。任何部分提交都必须可识别、可查询、不可绕过。查询或重放本身不是清理完成证明。

**验收**：分别注入目录读取失败、单记录不可读、损坏 JSON、Accepted 成功后 desired 写失败、各写入边界崩溃；重启和原进程内重试均不能产生幽灵操作或放开未知执行。

### R05 · P1 · 已观察到编排进程退出，后到成功 Done 仍被认定启动成功

**证据**：`crates/file-server-userapp/src/service/userapp/start_events.rs:122` 无条件 `Done => done_outcome(failed)`，不检查之前设置的 exited。与 runtime ownership Plan §阶段一的“启动完成提交前已观察到退出，不得仅凭缓存 Done 成功”冲突。

**触发**：按顺序送入 `ProducerExited`、`Done { failed: [] }`，当前消费者返回成功。进程已退出却可能向用户报告启动成功。

**修复**：保留短排空窗用于收集末尾事件与错误，但成功判定同时满足当前进程存活/完成提交契约。区分成功提交前退出与成功提交后健康变化，不无条件接受迟到成功。

**验收**：exit 0/非零→成功 Done；退出→失败 Done；EOF→Done；正常 Done 提交后退出；无 Done 且后代持有 stdout。检查事件顺序、错误原因和有界结束。

### R06 · P1 · workspaceType 同步仍混用 serviceType，Git body/query 会定位错误目录

**证据**：

- `crates/file-server/src/extract.rs:85` 新 header 缺失时回退 `x-service-type`。
- `models/computer.rs:37` 等把 workspaceType 作为 service_type 的 serde alias，canonical 字段仍是 serviceType；两个独立语义合成了一个字段。
- `handlers/git/mod.rs:143`、`:178` 构造 service context 仍取 service_type。appId 会激活 context，`:117` 优先进入 computer 定位，使传入的 workspace_type 未参与这一分支。
- 当前 TS `src/utils/computer/workspaceContext.js:17` 明确 serviceType 仅用于运行时路由，定位使用 `x-workspace-type`→body/query workspaceType→taskAgent。

**触发**：

1. 无新 header，旧 `x-service-type=userapp`，body.workspaceType=normalProject：旧 header 错误抢占目录类型。
2. Git body/query 给 workspaceType=normalProject、appId、userId、cId，不给 header/serviceType：context 被 appId 激活，但 kind 为空，可能落入 taskAgent 路径。
3. 同时传 serviceType 与 workspaceType：模型可能因同一反序列化字段重复而拒绝，而上游认为它们是独立字段。

**修复**：定位契约真正改成 workspaceType，运行时字段不参与目录选择；检查 Computer、Git、multipart、模型、OpenAPI 与示例。保留普通 Computer 的用户隔离，不要为了 UserApp 去用户化一起删除 Computer 用户语义。

**验收**：header/body/query/multipart 的四种类型矩阵；新旧字段并存/冲突；未知 header 类型回落规则；Git 读写请求；检查最终真实路径而非仅请求成功。

### R07 · P2 · UserApp 路径占位 user_id 仍被提取、校验

**证据**：`crates/rcoder-proxy/src/service/handlers/dev_terminal.rs:55` 仍接收 user_id，`:64` 调用 validate_identifier，`:134` 的 require_user_id 被多个工具入口使用。UserApp 去绑定 Spec §2 要求该段仅参与路由匹配，不提取/校验或定位。

**后果**：例如非空占位 `legacy.user` 在匹配 URL 后仍被旧 identifier 白名单拒绝；虽然纯 app_id 定位已改，旧用户值仍影响业务受理。固定 `0` 的正常流程覆盖不到这一点。

**修复**：保留 URL 路由段，删除业务层用户占位参数及校验，清理过时复合键注释；覆盖 app proxy 与 ttyd/vnc/audio/ime/dbx 等共享路径。保持 app_id 校验和普通 Computer 身份契约。

**验收**：同 app_id、多种非空合法 URL 路径段均到同实例；无需 x-user-id；dev/prod 隔离、Computer 原行为不变。

### R08 · P1 · 删除 user_id 后缺少持久 JSON 升级兼容

**证据**：`14615243^` 的 `UserAppResourceBinding` 包含 user_id；当前 `crates/shared_types/src/userapp/resource_binding.rs:7` 保留 deny_unknown_fields，却已删除字段。`crates/rcoder-storage/src/userapp_lifecycle/sql.rs:10`、`:33` 直接按新类型反序列化数据库记录。当前新增迁移 `migrations/0006_drop_userapp_metadata_user_id.sql` 仅删除 metadata 列；PG/SQLite lifecycle 迁移仍止于 0005，没有该 JSON 清理。

**后果**：旧数据库 binding JSON 含 user_id，新版本读取返回未知字段错误，升级后历史绑定/保护流程失败。全新测试库通过无法证明升级安全。Spec §5 明确要求覆盖持久 JSON；“不迁移旧容器”不等于可以不兼容旧数据库记录。

**修复**：清点本次字段删除影响的所有持久 JSON，增加 PG/SQLite 迁移或严格限定旧 schema 的读取升级。只移除退役字段，保留 app/lifecycle/physical UID、操作和历史删除保护；不能通过清空记录或全面关闭严格解析解决。

**验收**：使用变更前真实类型生成 fixture；已有绑定、未终态操作、已删除生命周期升级后仍可读且隔离有效；未知无关字段仍按契约处理；迁移重入及失败可恢复。

### R09 · P1 · kube watcher 错误后立即重试，没有声明的退避

**证据**：`crates/docker_manager/src/runtime/k8s_observation.rs:183` 直接构造 watcher；错误分支记录后立即下一次 poll，无 backoff 包装或延迟。代码注释写“watcher 自带退避重连”，但本地 kube-runtime 4.2.0 `watcher.rs:775` 明确说明恢复发生在 next poll，通常立即发生，延迟需要调用方使用 StreamBackoff 等机制。`:65` 的分类仅把 401/403 视为 fatal，其余全部重试。

**后果**：持续快速返回的 429/5xx 会在业务 deadline 内高频 LIST/WATCH；不可恢复的协议/解码错误也可能循环直至超时。总时间有界不等于请求频率有界。

**修复**：采用版本匹配的有界退避，等待仍受统一 deadline 和取消控制；先区分 fatal/transient，不把所有错误都交给重试。明确 410、EOF、断连、限流、权限、协议错误语义；根据本版本实现核对 Retry-After 支持，不假定库自动处理。

**验收**：受控 HTTP 服务连续返回 429/500，断言请求次数与间隔；重试中取消/超时及时结束；401/403 快速失败；不可恢复协议错误不热循环；410/EOF 恢复后不重置 deadline。再跑真实 K8s 对应套件。

## 3. 未完成范围与缺陷分开处理

- kube-runtime verification 明确仅交付批次 A；B（Builder 双资源观察）未做，C/D 为后续/可选。不能将未实现批次称为回归，也不能宣称完整接入已完成。先修 A，再独立推进 B。
- runtime ownership 阶段二已有新增 API/内核，但 profile、稳定锁根、attach、平台切换等必须逐项对照 Tasks；接口存在不代表阶段二/三完成。
- 阶段一启动预算当前实现和 verification 留有合法慢启动预算限制；后续补完整预算推导与配置测试，不能随意统一改成 300 秒。
- file-server 的 shared-skills manifest 同步需要单独核对原交接范围、实现和验收，不应把 workspaceType 部分提交当整个同步任务完成。
- 现有 Tasks 勾选与 verification 覆盖落后于代码。修复时按真实完成范围更新，不批量勾选，也不复制旧测试结果。

## 4. 建议修复顺序与交付门槛

1. 等正在执行的集成测试产出完整报告并固定其源码/镜像身份；当前审查不是该次测试结论。
2. 优先 R01–R04：统一 runtime 控制权、操作身份与持久化。如果无法本批完整实现，对未接线能力明确关闭/拒绝，避免提供虚假成功；这只能作为止血，不能算需求交付。
3. R05–R08：启动终态、目录定位、用户占位、升级兼容，按功能分提交。
4. R09：观察错误分类和退避，单独提交，默认与 kubernetes feature 分别验证。
5. 聚焦回归完成后按 AGENTS.md 做 Compose 与真实 K8s 验证；smoke 不能代替 userapp/gateway/chat 中实际受影响场景。新失败与已证明的基线失败分开列出。

每项交付包含：修复前反例、修复点、测试命令及退出码、证据路径、当前提交、部署镜像身份（有部署时）、剩余问题。禁止通过删断言、降低身份校验、清空存量数据库或自动 fallback legacy spawn 消除错误。

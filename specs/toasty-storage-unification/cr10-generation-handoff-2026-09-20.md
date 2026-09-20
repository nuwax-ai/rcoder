# CR10：凭据冷换代的制品与代次交接

## 问题与范围

固定快照 Compose 报告 `tests-e2e/reports/750d8ede9d7a4bd3a0c973251bf0ab51` 中，七服务启动及热部署成功，但显式配置 Restart 在约 3 秒内转入 RecoveryRequired。新容器 generation 已改为新 operation，卷上 journal 仍属旧 generation；app-cli 在配置激活门之前拒绝恢复，而平台已修改 PG，随后激活 ACK 失败。

修复覆盖运行中 Restart 与已经停止的显式 Start。停止的旧容器无需先启动业务，也不依赖管理 HTTP 在线。本方案不删除 journal、不放宽普通 generation 检查、不重新下载旧 env URL，不清理未知操作或迁移回执。

## 两阶段协议

1. 平台保有原生命周期操作及运行时租约，在 `execute_update` 捕获原物理 mutation target 后读取旧 generation。仅当前操作已注入新 generation/config 且未提交新制品的替换创建交接授权。
2. 授权含协议版本、app/lifecycle、新 operation/generation/config version、旧 generation 与旧 workload UID/name。它同时进入原操作 checkpoint 和平台保留环境变量 `APP_RUNTIME_GENERATION_HANDOFF`。运行时仍用原 mutation target 做条件替换；授权不是另一个删除或接管入口。
3. 新 owner 获取锁、成功 bind、确认旧业务已收束、完成内核 recovery 后才处理交接。核验 APP_ID、目标 generation、旧 journal generation、Active/StartupFailed 边界、active artifact 与真实 release.lock、确定的 execution target；检查 owner 与执行目录的迁移回执，任一未知/损坏即拒绝。
4. 读取真实已确认制品，保留其 release.lock 原字节和 active request，不使用旧 URL seed。生成 prepared 证据：原授权、artifact release ID、release 文件 SHA256、旧 journal SHA256、执行目录、原 desired revision。
5. 同一个 journal 原子写入新 generation 和 prepared 回执。以记录的 revision 将 desired 更新为 Running；两文件之间崩溃只能重放同一个 revision，不生成新的启动授权。二者确认后才公开 prepared。
6. 平台经新 generation 的物理目标绑定管理通道读取 prepared，逐字段核对原 checkpoint 和摘要格式。**通过后才开始 PG 管理探测及凭据修改**。身份错误、明确拒绝、响应损坏、超时/断连均不执行 PG SQL。
7. PG 应用和验证成功后，沿用既有 activation 协议。ACK 未知仍保留 Applied/Unknown 与原操作保护，不回滚凭据、不重试新操作。

授权属于可信平台注入；新 owner 不以自行扫描同名资源来证明旧 UID。旧 UID 的操作权限由平台原租约和运行时条件替换保证；返回通道由平台新物理 UID+generation 核验保证。

## 一次性授权和 Stopped

- 首次显式交接可从 Stopped 转 Running，只推进被 journal 绑定的 desired revision。
- 后续 Stop 推进 revision，历史容器 env 不再产生新的启动意图。
- activation 已持久确认后，交接授权已消费；后续同代热部署可替换旧 journal，普通 owner 重建走原有 journal、制品、迁移及 desired 检查，不再强求旧制品摘要。
- 自动恢复在写 Switching 之前判断 Stopped，避免没有实际启动却留下 Switching。
- 未确认的内核操作仍由原 recover 流程阻断；交接不能绕开恢复保护。

## API 和拒绝语义

`GET /v1/runtime/configuration/prepared` 使用原部署 token，公开 OpenAPI：200 返回 prepared；202/HANDOFF_PENDING 表示 owner 尚未完成准备；403/409 表示授权或恢复保护拒绝。平台在总 operation deadline 内观察 pending/连接暂态，明确管理端拒绝立即结束。prepared 不等于 PG 已应用，也不等于业务 Ready。

activation 已落盘但控制面尚未收束时 owner 重启，进程内 prepared 缓存为空。此时同一端点从持久 activation 返回 `{activated: true, authorization: ...}`，表示已消费的原身份完成证据；同 activation POST 幂等返回，不要求重建 prepared。平台核对原授权及物理目标，且持久配置记录必须已为 Applied，随后只观察 `/ready`，不再探测或修改 PG、不再 POST activation。该只读回执不授予新的启动意图。

## 本轮反例（实现，尚未执行）

app-cli：

- 运行与 Stopped 两路径都承接热部署 B 和 `.run`，保持 release 原字节与原 operation identity，重放不再推进 revision。
- Switching、错误 generation、错误 artifact、未知迁移、恢复保护均拒绝，原 journal 与 desired 不变。
- journal 已转代但 desired 尚未写入的崩溃重放，仅完成原 revision。
- 后续 Stop 不复活、不先写 Switching。
- activation 已消费后，后续热部署替换 journal，再 Stop 和重建，不要求旧制品、不复活。
- activation 持久化后丢弃整个 owner，重建空 prepared 缓存，通过真实本地 HTTP GET 读持久 ACK、POST 同身份幂等确认；desired 保持 Stopped、journal 保持 Active。

app_manager：通过 `complete_cold_runtime_configuration` 验证错误 operation、错误旧物理 UID、明确管理拒绝及断连，所有管理命令只访问 prepared；credentials 保持 Captured，business 保持 NotStarted，没有 PG SQL。另覆盖已消费 ACK 的原操作恢复：仅 prepared/ready 两次只读管理调用，Applied/Unknown 收束为 Applied/Ready，无 PG 和 activation 写入。

## 验证状态与后续门禁

阶段提交 `432fe129` 保存了初始 WIP；本文件对应其后续完善。当前仅完成源码核查、指定文件 rustfmt 与 `git diff --check`；遵循统一验证安排，未运行 Cargo 或 Compose。

待统一执行 app_manager/shared_types 聚焦 nextest、app-cli 独立 nextest/Clippy；再用固定源码快照重跑 Compose 完整链，明确包含“热部署 B → 凭据 Restart → 保持 B → Stop → 带凭据显式 Start → 保持 B”与 activation ACK 未知保护。只有部署报告通过后才可标记 CR10 验收完成；真实 K8s 替换仍需对应集群验证。

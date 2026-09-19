# 第八批提交复核

基线：RCoder 166c3579；只读源码复核，未修改业务代码，未重跑 Compose/K8s 或三平台测试。既有独立复核报告属于其他执行者提供的证据，不冒充本轮实跑。

## 1. Pingap 修复正确，但交付范围仅限该修复

make/docker.mk:195 的版本/revision 已与 crates/app-cli/src/devtool.rs:18-19 对齐。166c3579 只改此构建 pin 和三份记录，没有修复四个剩余场景或 Turso R01–R06。双架构 tarball 是本地忽略缓存，不是提交里的可复现产物；构建仍应通过已有下载路径获取。

建议补一个版本一致性门禁，覆盖 app-cli Cargo pin/devtool fallback、docker/build-app-runtime.py、make/docker.mk、配套生产构建变量；若显式允许差异必须记录理由。镜像验收记录实际 pingap --version、二进制摘要和 app-cli 身份，不只看源码版本。

## 2. 新发现：失败后排队槽可能滞留并在更晚成功后反向派发（P1，源码路径确认，未运行反例）

crates/app-cli/src/runtime_kernel.rs:1055-1075 和 1130-1150：active 终态会清 active_operation_id，但只有 Succeeded 才 take pending_restart。Failed/Cancelled 会留下原排队请求。

同文件 943-965：有 active 时新请求替换排队槽；无 active 时新请求只占 active，没有清理之前滞留的 pending_restart。

时序：A 执行、B 排队 → A Failed → B 仍 Accepted 且未派发 → C 新受理成为 active → C Succeeded → 旧 B 被派发。这违背最后受理生效；B 无后续请求时也可能长期无终态。此问题与报告中的“失败重试瞬时 completed”不能直接画等号。

修复要求：明确 active 已确认失败/取消时排队者的终局策略，持久化收束并清槽；若选择允许继续执行，需要先确认运行态已安全收束并检查最新意图。RecoveryRequired 仍保持保护，不借失败恢复放开未知写。新请求不能让更旧请求在自己成功后执行。

专用反例：A+排队B，A分别 Failed/Cancelled/RecoveryRequired，再提交C；断言派发顺序、B终态、当前身份和服务代次。同时保留 Stop 优先与排队覆盖持久化失败的保护。

## 3. 结构化 blocker 缺失仍成立（P2）

crates/app_manager/src/service/mod.rs:179-191 仍将进程锁抢占失败转换为普通 Conflict 字符串，无法满足 M3 blocker 透传。

不要直接把消息硬编码成一个虚构 operation/blocker。应在权威受理/锁元数据处取得真实 app、scope、operation；处理持锁但尚未 durable admission 的窗口及查询竞态。避免为补错误信息改成等待锁或旁路互斥。覆盖同进程快速冲突和跨副本存储冲突两条链。

## 4. flock 根因尚不能仅凭报错定案

docker_builder_deletion.rs:440-448 的 Drop 会 unlock，:474-484 的 release 也显式 unlock；builder_completion.rs:30-46 会在成功/明确拒绝时 release，未知结果 drop 但保留 marker。因此“lock would block”说明仍有互斥锁持有者，不能单凭它证明 ensure 泄漏 fd。

进一步记录精确锁路径、device/inode、PID、operation_id、持有任务的开始/结束、release 结果；沿 Docker runtime 创建协调任务排查未结束/重入/其他进程。用干净 app ID 重现 ensure 完成后立即 destroy。禁止直接删 lock 文件（可分裂 inode 锁域）、强制解锁、吞 Conflict。

## 5. 其他未闭环场景

- two_users：用独立新 app ID 和隔离数据目录重跑；如需清理，只删除证据确认属于本次测试的资源。lifecycle 冲突本身可能是正确保护，不能削弱校验。未重跑前不能算通过。
- deploy_full_chain：尚未定位 file-server 连接失败层级。逐层记录 prod 容器 UID/镜像、进程、监听端口、容器内直连、RCoder 到容器连接、代理请求与响应。区分连接拒绝/超时与 N07 token 401/403，不能把认证失败当网络不可达。
- 瞬时 completed：人为制造可控启动失败并恢复，记录 file-server release_id→app-cli operation_id→admission replay/queue→事件序号→真实进程/ready；同 ID 重放应返回原失败，新 ID 应有真实执行或明确拒绝。不能把现象直接归到 R02，也不能通过无条件重复编排修补。

## 6. Turso 前轮阻断项仍未修复

最新提交未涉及 Turso 源码。Claude 独立报告已记录 R01/R03/R04队列/R05 的反例复现；R02 关机顺序和 R06 配套遗漏同样未修。先修 worker/锁生命周期和关机收束，再补事务/关闭/目录/配套问题，不能因本轮 Pingap 成功而略过。

Claude 复核报告中一个需要纠正的推论：shutdown 的 take handle 到 send(true) 之间没有 await，普通 Tokio future 取消不会在这一同步片段任意插入；不能把该特定窗口当成已证实的取消缺陷。并发 shutdown 提前成功、join panic 被吞及需要共享完成结果的结论仍成立。

## 7. 验收边界

只针对本次 Compose 构建 pin 改动，不重复无关 K8s 全套是合理的。随后若修共享 app-cli 队列/运行内核、RCoder 关机或公共错误契约，就应补相应 PG/K8s 验证，不能沿用“本轮 K8s 无需重跑”作为永久豁免。

当前交付仍是局部修复：4 场景、失败重试、Turso阻断项、NT三平台完整矩阵、N07/R02剩余语义分别跟踪。修复后先聚焦反例，再完整 Compose；报告源码/镜像身份、逐例结果、实际未运行项。

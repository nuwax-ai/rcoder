# UserApp 生命周期收敛规范

状态：用户已批准实施；任务完成与验证证据分别登记在 tasks.md。

## 目标

解决真实 K8s 三副本验收中的首次 PVC claim 冲突、错误分类丢失、purge 元数据假冲突与操作锁残留，并使同类跨进程问题可恢复、可验证。

## 行为不变量

1. app_id 是业务定位键；lifecycle_id 是应用存在周期的身份，仅彻底删除后显式重建才改变；metadata_revision 只用于业务字段并发更新。均不复用 release_id、K8s resourceVersion 或 app-cli deployment_generation_id。
2. 同 owner 的重复 workspace ensure 不创建新生命周期，不覆盖不属于该命令的元数据字段。不同 owner 的普通 ensure 拒绝；归属迁移需要独立命令。
3. 归属、生命周期状态、删除许可由权威存储判定。缓存不能否决已确认提交的删除，也不能允许 Deleting/Deleted 状态接受旧写入。
4. 同一应用的冲突生命周期操作统一受理；重复同操作可查询/等待原操作，不重复执行。长任务的取消/结束协议仍需与 purge 协作。
5. 远端创建、更新、删除绑定真实资源身份。resourceVersion 冲突仅在 UID/生命周期/归属不变时有限重试。
6. HTTP 超时不代表操作失败或取消。操作状态必须区分失败、处理中、结果待核实、完成；既有同步接口不得提前返回完全成功。
7. purge 持久化意图后再产生副作用；逐步记录进度，重复推进幂等；不能确认远端写入结果时阻止竞争的新操作。
8. 权威存储不能确认安全前置条件时，破坏性操作在产生副作用前拒绝。普通展示元数据可保留显式降级，但降级补写也受生命周期前置条件约束。
9. Deleted 保留旧生命周期墓碑，普通旧 ensure/补写不能恢复它；显式新建生成新 lifecycle_id。墓碑不按任意短 TTL 清除。
10. 跨 crate 的领域类型、命令和结果定义在 shared_types；生产实现无 unsafe、无 unwrap/expect，不持 DashMap guard 跨 await。

## 范围

统一覆盖 builder ensure、owner 登记、生产 create/update/start/stop/hot 控制受理、分级 delete/storage destroy 和整应用 purge。开始实施前固定每个入口的实际删除范围矩阵；普通 prod delete 不得误升级为整应用 purge，agent PVC 永不进入删除路径。

保留当前正式 HTTP 路由、参数位置、HTTP 200 + HttpResult 和同步成功含义；允许增加操作状态查询与可选显式 request_id，但不强迫现有调用方立即迁移。保留容器内 hot 行为、现有部署身份协议与已删除的容量/条目限额。

## 非目标

本轮不引入 CRD/独立 Operator、Kafka/Redis/Temporal、跨 K8s/PG 两阶段提交或完整事件溯源。复制控制器的持久化意图和幂等推进思想，不复制整套平台。不能承诺任意未知远端写都能自动、安全地接管。

## 已批准的存储与首开契约

- Compose 单 rcoder 使用 SQLx SQLite，仅持久化 userApp；K8s 使用 PG，Agent 存储配置独立。
- SQLite WAL/FULL/foreign_keys，5 秒忙等待，初版单连接；事务内不执行 runtime I/O，失败不降级为内存成功。
- 同身份同配置 ensure 等待同一操作，默认 90 秒总预算；类型冲突、删除和不确定操作不能当创建重试。
- 锁消失或探活通过不能代替操作结果、资源归属和生命周期校验。
- 三份 Compose 挂载独立数据目录，数据库目录永不进入 userApp purge；正常上传无限额，错误收尾至多 1 MiB/1 秒。

# 即时改密的远端事务回执与恢复边界

## 目的

控制存储中的 WriteSubmitted 无法证明远端 PG 命令是否提交。单纯 TCP 登录成功也不能排除迟到的旧请求：若先释放保护、允许下一次改密，旧请求可能随后覆盖新密码。因此恢复需要远端事务证据与迟到请求幂等保护。

这不改变 CR10 保存仅待生效的语义，仅用于显式数据库管理入口中的非受管理运行账号。受管理账号仍由运行配置流程控制。

## 已实现

- 每个应用 PG 的 postgres 数据库中保存 `rcoder_management.password_receipts`，复合键为 app_id/lifecycle_id/scope/operation_id，附 fingerprint、目标账号、原写事务 ID；不保存密码。
- schema/ACL 引导在单独短事务中串行化，结束后才进入改密事务。这个事务级 advisory lock 只解决并发 DDL/REVOKE 的目录元组竞争；业务正确性由下一事务的唯一约束和条件更新保证，不使用 SELECT FOR UPDATE。
- 改密事务显式使用 READ COMMITTED。插入回执时记录 pg_current_xact_id；冲突时只允许 fingerprint/账号一致，保留原 writer_xid。仅实际插入回执的事务执行 CREATE/ALTER ROLE。回执和角色密码一起提交，失败一起回滚。
- 迟到的同操作重放只核验原回执，不再次 ALTER；同身份改参拒绝。另一操作改密之后重放旧请求，不覆盖较新的密码。
- 原控制协调器在执行成功后，另查完整匹配的回执，再 TCP 验证，最后记录 Verified/Succeeded。命令失败、回执查询失败/缺失、TCP 失败均不能假成功。
- 密码只通过事务局部参数进入 SQL，SQL 字符串使用显式 E 字面量处理反斜杠和单引号，shell 层独立引用。命令使用会话级日志参数，避免默认 PG 错误日志回显含密码的复合 SQL；自定义审计插件未验证，不能据此保证任意外部日志配置都不记录私有输入。

使用短事务不能消灭等待。唯一索引冲突可能等待原事务结束；命令配置 statement_timeout 与 idle_in_transaction_session_timeout，外层沿用总 deadline。设计依据：[PostgreSQL INSERT/ON CONFLICT](https://www.postgresql.org/docs/16/sql-insert.html)、[READ COMMITTED](https://www.postgresql.org/docs/16/transaction-iso.html)。

## 明确未完成

1. 显式恢复 API 已接入原身份、原物理目标、回执及控制存储 CAS；完整 HTTP/容器端到端验收仍未完成。
2. 缺失回执不能推导没有写入，也不能释放保护；旧请求可能尚未到 PG。数据库层现已实现取消回执/墓碑（已接入显式恢复协调器）：与原写请求竞争同一唯一键，若取消先到则禁止迟到写入；若原写先提交则返回已提交，不冒充取消。
3. 取消/已提交结果必须进入不同的终态证据，不能把取消记为改密成功。未知的旧部署没有回执时保持保护，不用时间窗口猜测。
4. 回执不能自动过期删除；删除后迟到请求将失去幂等保护。未来清理必须有生命周期及所有旧执行通道已关闭的证据。
5. 回执只证明这个 PG 事务，不替代 Pod/container UID、代次、lifecycle、执行者、租约和控制记录 revision 校验。备份、迁移后的恢复需要独立物理身份核验。

## 取消墓碑实现

新增 outcome=committed/cancelled。已有回执添加该字段时默认 committed，保留原先已提交写入证据。取消只插入 cancelled 或返回完整身份匹配的已有 outcome，不覆盖原 outcome。原写请求发现 cancelled 时事务失败，不执行角色 SQL；成功回执查询只接受 committed。

取消命令退出非零、超时、返回空结果或未知结果均不是安全释放的证据。即使收到 RETURNING 行，也必须等待整个命令成功结束（COMMIT 完成），随后在控制层按原身份和版本提交终态。恢复 HTTP 入口已调用该 helper；完整真实部署验收仍待完成。

## 本轮证据

- shared_types 的 pg_utils 测试 21/21 通过，run `f7bbec4a-a124-4c3e-8f52-e561677b8534`，包括非法输入拒绝及实际 shell 参数往返；不是整个协调器验收。
- 个人集群 namespace `nuwax-k8s-test`、primary `nuwax-k8s-test-pg-2`，实际 PG 17.9。使用 `tools/test_pg_password_receipt.py` 创建 UUID 命名的临时数据库/账号，未修改已有业务库。
- 创建、后续改密、旧请求迟到重放不覆盖、改参拒绝、SQL 失败回滚、连接结束未提交回滚、真实 TCP 正误密码、两个独立连接并发重复请求全部通过；本次创建的库和角色均已清理。
- 初版并发测试两次复现 `tuple concurrently updated`，定位为重复 schema/ACL 引导；拆分短引导事务后上述并发场景通过。
- 尚未验证应用镜像 PG 16、完整 RCoder HTTP、Compose/K8s UserApp、Pod 换代及恢复 API。

## 控制存储恢复提交

新增 finalize_password_recovery(snapshot, evidence)：在与受理/删除相同的应用根 CAS 短事务内重读，要求完整 snapshot 一致、状态 Running/RecoveryRequired、原执行者及租约存在、原账号及物理目标不变。只接受 WriteSubmitted 的确认结果，或一致的 Verified/Cancelled 终局证据；禁止颠倒已确认的结果。

该方法复用有序证据校验与槽位释放规则：Verified 写 Succeeded，Cancelled 写 Failed。原租约记录继续保留，供既有终态租约扫描按原 receipt 清理。它不执行 PG 命令，不按租约年龄判断安全，不生成新 operation_id/executor。调用者必须先经原物理通道确认 PG 回执，并在成功分支完成 TCP 验证。

显式恢复 HTTP 协调器已调用该方法。新写入证据带 receipt_protocol=1；缺失或未知版本拒绝恢复，防止用新墓碑推断旧写入已被阻止。恢复提交前确认取消事务退出成功；committed 分支还需原密码 TCP 验证。终态之后按原 receipt 清理，清理失败返回 lease_cleanup_pending=true。端到端部署验收仍未完成。

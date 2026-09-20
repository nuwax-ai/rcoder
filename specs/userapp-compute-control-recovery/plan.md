# 技术方案

## 存储

保留四个已发布 v1 SQL 及其 checksum。userapp 增量 v2 仅创建独立控制意图与操作表；迁移与账本同事务，历史版本逐条验证，拒绝缺口、未来版本、约束漂移。PG/Turso 共用相同 DDL。新增控制 API 必须和 userapps.control_revision CAS 共用竞争点，受理与删除/重建不能各自孤立。

控制操作在自身行保存不可变的 `lease_json` 及独立 `checkpoint_json`，不写入带普通 operation 外键的 `userapp_operation_leases`。绑定回执、推进状态、完成及清理回执均参与根 CAS，并核对 lifecycle、operation、executor、代次与 revision。终态扫描使用运行时的精确回执释放，再按相同回执清理账本；历史操作清理不得更新当前控制意图。`Superseded` 只代表旧执行者失去授权，不允许据此释放回执。旧执行者可以提交自己的收束证明，未知远端写不能用超时/中止 future 代替完成证据。该证明尚需由运行时协调器接入，组件测试不是运行时实现。

控制受理不得直接替换普通 active_operations 槽：普通执行仍需携带原身份收束。控制意图中的 generation 是拒绝旧提交的条件，不是旧远端写已结束的证明。实际运行时变更必须有 in-flight 登记及可核验回执；在途创建不得仅凭当前 404 判完成。

幂等信息直接保存在控制操作记录中，不创建独立 compute_requests 表。同一原始 request_id 返回原操作；不同 request_id 遇到进行中的控制操作立即返回结构化 OperationInProgress（HTTP ERR_CONFLICT），不新增记录。Stop 覆盖 Restart 是唯一优先级例外。移除 after_operation_id、等待队列和自动提升后续操作的逻辑。停止完成后的启动需要用户重新发起。

## 协调器

Stop 高于 Restart；重试返回已有控制操作，Stop 覆盖 Restart 并保留被覆盖操作历史。停止意图受理即持久化，完成需原实例退出证据。跨副本恢复按原操作继续。scope 为 dev/prod，application 删除优先且不可复活。

UserApp Stop 缩容到零；Restart 收束旧运行后恢复已确认配置。保留卷、禁止新旧实例同时访问工作区。prod 只启动已确认制品；管理通道/PG/业务 Ready 分开，迁移未知不重跑，改密不回退。

## 自动登记

创建应用身份前先扫描资源；实时验证控制器、PVC 及 lifecycle，dev/prod 共同核对。匹配则导入旧身份与绑定；停止标记保持。旧数据库丢失不等于旧任务成功或已取消。当前有新资源、有删除墓碑或多个冲突候选时进入可查询恢复，不改标签绕过身份验证。

## HTTP 与交付

UserApp stop/restart 返回 202 和 operation_id、stage、status_url；Java 须支持 202 并透传 blocker，完成状态通过查询取得。普通 agent 路径保持。先组件反例和真实 PG，再 Compose、个人 K8s；最后用已发布新接口处理 app 129。

首次部署停止旧协议 writer 后统一升级，不能让旧二进制操作新控制状态。增量升级无需清库。保留无关工作树变更，不自动提交或发布。

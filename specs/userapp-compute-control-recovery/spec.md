# UserApp 高优先级计算控制与资源恢复

## 已确认行为

- Stop > Restart > 普通业务操作；dev/prod 独立，删除墓碑不被恢复撤销。
- 普通操作占槽及 RecoveryRequired 不阻止 Stop/Restart 受理；受理不等于完成。
- UserApp stop 缩容至零；restart 先确认旧实例退出再启动；PVC、PG 数据保留。
- prod 恢复已确认版本，不隐式发布、改密或重跑未知迁移。
- 正常重新部署复用生命周期；控制记录缺失时先发现并核验现存资源再创建身份。
- 自动恢复登记不自动启动已停止实例。显式新对话可在 stop 完成后启动 dev。
- 普通 agent 既有重启行为不在本次变更范围。

## 事故反例

app 129 清库后，新生命周期与旧 builder 不一致。EnsureBuilder 因身份冲突进入 RecoveryRequired；删除旧 Pod/STS 后持久 dev 槽仍占用。聊天、stop、restart 都无法进入执行。

## 完成标准

真实调用链证明多副本停止优先、旧代次不能提交/误删、未知写恢复、现存资源接管、数据保留；组件、Compose、K8s 与 Java 联调分别报告。禁止清库或直接清锁制造通过。

## 后续显式启动语义（最终确认：不排队）

停止不是永久禁用。Stop/Restart 未完成时，新的 Start/Restart 立即返回
ERR_CONFLICT 和当前操作 ID、阶段；不保存待执行请求，不在内部自动接续。
完成后，用户重新发起的新 Start/Restart 正常受理；被拒绝的 request_id 没有
占用幂等记录，可以在操作结束后重新提交。同一已受理 request_id 的重试返回原操作。
保留 Stop > Restart：新的 Stop 可中断正在执行的 Restart。其他新控制请求遇到
进行中的控制操作均返回冲突；不把一次新点击作为已执行成功。
旧执行者围栏仅约束被中断的操作身份，不阻止新身份操作。
自动探活、状态查询和后台扫描不能自行唤醒 Stopped。
本节替代此前“停止期间保存后续启动意图”的排队方案。

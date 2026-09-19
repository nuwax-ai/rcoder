# 管理准备未知结果的显式核验恢复

## 修复范围

PrepareProdDatabase 在 StartSubmitted 后物理启动成功、管理观察失败时，原操作保留 RecoveryRequired。此前显式 retry 只支持 Pending 或已有最终证据，无法核验迟到成功。

现有 retry_control_operation 增加专用只读分支：复用原 operation/lifecycle/executor 与租约、prod 槽位，核对原 workload UID/name/kind 与 deployment generation；通过绑定管理容器读取 PGDATA 管理员标记并执行 socket SELECT 1。观察结束再次核对 workload。不会重新发起 start/wake，不提升 pending 配置。

正证据经 UserAppLifecycleStore.confirm_database_preparation_recovery 的完整 snapshot CAS 提交 ManagementReady（原身份 Running，槽位与租约保留），再进入既有最终证据结算与条件租约释放。Turso/PG 共用同一事务实现。超时、断连、错误物理身份、代次变化、过期 snapshot 都不解除原保护；若提交后观察方退出，下次通过已有最终证据恢复。

## 新增反例

- 物理启动已成功、首次观察断开；再次观察仍失败不改变记录，迟到管理成功后原身份收束且启动次数始终为一。
- 相同名称换 UID、部署 generation 改变均拒绝，原 lease/slot 保留。
- 存储拒绝 Captured 跳步、过期 snapshot、错误目标；确认 ManagementReady 后仍持有原租约与槽位，旧 snapshot 不可重复提交。
- 成功恢复不改变 runtime_policy。

## 本轮证据边界

本轮仅执行定向 rustfmt 与 git diff --check，退出 0。按集中验证要求未执行 Cargo、Compose、K8s，新增测试尚未证明通过。建议集中运行 app_manager 的 management_recovery 与存储的 preparation_recovery 筛选，再执行相应默认/完整 feature 检查及真实运行时回归。没有清锁、重启部署、修改现有数据或提交 Git。

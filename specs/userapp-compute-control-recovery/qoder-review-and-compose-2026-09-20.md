# Qoder 复核与真实 Compose 验证

## 结论

基线 HEAD `447247b5`，叠加当前未提交的 compute-control 与 dev owner 修复。未提交、未推送、未修改 .18 集群。本轮不是整个高优先级容器控制方案的完成验收。

“agent 手动启动 app-cli，RCoder dev/stop 无登记而拒绝”已在真实 Compose **先复现失败，再验证修复成功**。Qoder 的交付仍有真实业务恢复失败及验收代码遗漏，不能按全部完成处理。

## 1. 手动 owner 场景：已通过

按需复现入口：

```bash
python3 tests-e2e/tools/manual_owner_stop.py --report /tmp/manual-owner-stop.json
```

它创建唯一测试 builder，写入轻量真实 HTTP 服务，用 app-cli gen-lock/build/serve 启动工作区内 `.local-deploy`，删除本次生成的来源记录以覆盖旧目录，随后经 RCoder 调用 dev/stop。没有模拟 owner 或 AI 响应。

- 修改前：9080 实际响应正常，RCoder 返回 `admin port ... held by a running app-cli owner ... no registration`。
- 修改后：返回 `Stopped`；业务 HTTP 停止；3010 同一 runtime_instance_id 仍存活；容器 ID、工作区文件保留。
- 清理只删除本次捕获 ID 的临时 builder，工作区卷保留。
- 新 agent_runner 镜像 ID：`sha256:ce4f2aa360923f3f946156fca474116c9137d18341769192771784b6f9dc5da1`。由本轮 Linux 编译二进制替换旧测试镜像中的 agent_runner 构成，app-cli 保留旧镜像版本，证明工作区内旧 owner 的停止兼容性；不是本轮完整 app-cli 镜像发布证据。
- 证据：[复现报告目录](../../tests-e2e/reports/manual-owner-stop-20260920/)。before/after JSON 均含实际容器、镜像、owner 身份与清理结果。

此脚本按需执行，不增加默认全套测试数量。跨目录 Stop 后再由平台切回源码启动、K8s 实际路径仍未验收。

## 2. 本轮修复的 Qoder 问题

### Q1：部署仍在执行时，恢复接口可能提前释放槽位

`db_password/deploy_recovery.rs::validate_deploy_pg_snapshot` 和存储 `common/ops.rs::finalize_deploy_pg_recovery` 原本都允许 Running。PG 回执只证明密码 SQL 的结果，不能证明部署协调器后续业务等待、策略写入与迁移 SQL 已结束。

两层现在只接受 RecoveryRequired；终态幂等重放保留。现有两个测试扩充 Running 拒绝断言，存储层同时断言原记录未改变。没有增加测试函数数量。Running 崩溃后的完整执行收束仍需控制协调器，不能直接调用此接口清锁。

### Q2：E2E 把 health 查询成功当成唤醒成功

旧 `verify_stop_and_wake` 只判断 HTTP 200/成功信封，Stopped 的查询响应也会通过。现改为：手动 Stop 后 health 查询仍停止；随后显式 Start，验证真实代理 HTML 和容器运行状态。同步两份必测清单。

### Q3：CR10 验收清单与用户最终语义不一致

`acceptance_steps.json` 遗留九条“保存待生效、凭据换代”断言。用户已确定立即改 PG、业务重启由用户决定，因此替换为现有立即改密、连接保持、dev/prod 隔离及停止态治理断言。保留本轮实际业务失败，不用删断言制造通过。

### Q4：两个必测步骤只在失败时写报告

`CR10 governance lifecycle readable`、`CR10 governance account readable` 原成功路径不写断言，严格聚合必然报缺步骤。改为成功/失败都记录实际结果。

### Q5：固定 revision=0 导致恢复拒绝测试证据不足

“完成部署拒绝恢复”原请求固定 revision=0，可能只证明版本过期。现先读取同 operation 的真实 revision，要求状态 Succeeded，再验证恢复拒绝。沿用原断言名和用例。

### Q6：同一失败重复消耗 180 秒

业务在改密前已未就绪时，改密后的“业务仍可用”保留失败结果，但不重复等待完整窗口。PG TCP、会话保持等独立断言仍执行。

## 3. 实际验证

| 验证 | 结果 |
|---|---|
| 容器缓存构建 rcoder、agent_runner release，hotpath/dial9，4 jobs | 退出 0，3m45s |
| 真实手动 owner 脚本，旧镜像 | 退出 1，精确复现原拒绝 |
| 同脚本，新 RCoder/agent_runner | 退出 0，服务停止、owner/容器/工作区保留 |
| nextest：rcoder + rcoder-storage，筛选 deploy_pg_snapshot / deploy_pg_recovery_finalization，默认与 all-features | 两轮均退出 0，各 3 个通过；其余为显式筛选，不计通过 |
| rcoder + rcoder-storage Clippy，all-targets，默认与 all-features，2 jobs | 均退出 0 |
| 根 workspace 与独立 app-cli fmt check | 退出 0 |
| Compose 原有 userapp_deploy_full_chain | make 退出 2，libtest 101；82 条实际布尔断言中 79 通过、3 失败，整体未通过 |
| Q3–Q6 调整后的原 E2E 编译及 CR10 清单/源码一致性检查 | 退出 0；未再次运行完整部署链 |

Compose 命令：`CARGO_BUILD_JOBS=4 E2E_SUITE=compose_userapp_deploy E2E_FILTER=userapp_deploy_full_chain make test-e2e`。

报告：[95f3886fab7d461dbdeb7456d9989e8e](../../tests-e2e/reports/95f3886fab7d461dbdeb7456d9989e8e/summary.json)。历史报告保持原结果，不按后续清单修正改写。

## 4. 仍未修复/完成的事项

### R1：编排中 Stop 后，Start 返回成功但业务无法恢复

本轮轻量部署代理页面可访问时，内部编排仍未结束。Stop 期间日志出现 `ABNORMAL_TERMINATION: app-pingap`、`SHUTDOWN_STATE`，随后容器 Start 的 app-cli 报 `deployment interrupted after switch; explicit redeployment required`。源码 `server_journal.rs::resume` 对 Switching/Activated/Failed 的保护仍存在。

失败的三个断言是：显式 Start 恢复真实业务、改密前业务就绪、改密后业务仍可用。PG 原地改密、新旧 TCP 验证、既有会话继续、容器/owner 保留、dev 隔离、停止态拒绝改密及重新启动后新密码保留都通过。

本轮 app-runtime 使用既有镜像 `sha256:01e0fdac22ea8bffe8b078b86ac91ff2098b579e3ad301959921751c7a1a80f9`，未用当前完整 app-cli 源码重建。因此这是该实际镜像组合的故障证据；新 app-cli 镜像仍须复核。

后续处理：在容器控制协调器中记录被打断阶段、确认旧计算实例退出；区分已确认制品与未知迁移，允许显式恢复已确认版本，未知迁移继续要求恢复处理。不能通过删除 journal 或只检查端口就放行。接口成功和业务 Ready 的状态要分别可查询。

### R2：热部署携带显式 PG 的先后顺序仍不完整

`deploy_control.rs::execute_deploy_input` 先等待 `try_deploy_via_container_api_with_guard` 返回，再应用显式 PG；`hot_deploy.rs::read_ready_operation` 要求业务 Ready。因此冷部署的 PG 前移不能证明热部署已解决密码导致的不就绪循环。

后续将热部署分为受理/切换与业务等待，使同操作、同物理目标的 PG 回执阶段先于依赖该密码的 Ready 等待；独立保存密码回执，避免后续 checkpoint 覆盖。需要利用原热部署用例加入一次带凭据的真实分支，不新增大量 helper 测试。

### R3：整体计划与平台验证

高优先级容器 Stop/Restart 的运行时执行器、在途写收束、自动资源登记恢复、202/恢复接口仍按 tasks.md 未完成。Qoder 的 Windows、完整个人 K8s、Java 联调也没有本轮完成证据。不得把本次 dev/stop 成功等同于上述能力已交付。

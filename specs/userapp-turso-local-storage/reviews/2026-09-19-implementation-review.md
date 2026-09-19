# 2026-09-19 实施审查与修复清单

## 基线与结论

RCoder HEAD：e70b3f33；主要实现提交 3db17134。审查开始时 RCoder 工作树干净。配套 build-agent-docker HEAD 为 4a9897b，Turso 配置与 RBD/STS 改造仍在已暂存/未暂存/未跟踪工作树中，不能当成该仓库已提交交付。

本轮重点是最近确认的 Turso 替换与 trait 设计，并核查 dev/prod 验收记录及 RBD 桥接交付边界；不是对历次所有 app-cli/代理提交的全量再认证。只读审查代码，新增本文，不修改业务源码/部署。

**结论：主结构成立，但有 P1 生命周期缺陷，尚不满足方案的全部验收要求。**

已经实现：默认 Turso、保留 PG/Kubernetes 门控、关键 trait 方法无 unsupported 默认实现、专用数据库线程、迁移版本校验、领域状态机复用、独立关闭接口、测试观察器改用 Turso。下列缺陷须修复。

## R01 / P1：初始化失败或取消可遗留 worker，目录锁先于 worker 释放

源码：`crates/rcoder-storage/src/userapp_lifecycle/turso/mod.rs:62-80, 98-128, 729-739`。

- open_exclusive 在 quarantine 成功之后才把锁与 worker handle 装到 store；quarantine 报错时，局部 handle 被丢弃、线程未 join，store Drop 因 worker=None 无法发 shutdown。
- watch::changed 返回 Err 被忽略；sender 已消失且值仍 false 时，biased select 会不断选中就绪错误分支，worker 无法正常退出，可能忙循环。
- 初始化 future 在 spawn_worker/迁移/quarantine 期间被取消，也没有拥有锁的线程级清理保障。ready_tx.send 失败被丢弃，同样继续运行。
- 正常 store Drop 仅发信号且不 join，随后锁字段释放，worker 仍可执行/排空已接收任务。第二实例可能在前一写入线程退出前获取独占锁。并非断言每次 Drop 都造成并写，而是保护不变量已被打破。

修复：锁从创建线程起转交 worker 持有，直到连接销毁、线程即将退出才释放；ready 接收方消失或 shutdown sender 关闭都明确走终止路径。任何启动失败都可回收线程，无绕过锁的后台任务。生产代码 runtime build 的 expect（第 94 行）改为显式初始化错误。

回归：控制屏障暂停初始化/事务，取消 open 或 drop store，验证第二实例在 worker 退出前必定拒绝、退出后能打开；损坏 quarantine 反复启动不增加活跃线程、不忙循环。不要只测正常第二次 open 被拒。

## R02 / P1：关闭数据库前没有等待业务执行者停止

源码：`crates/rcoder/src/shutdown.rs:104-114`、`main.rs:359-370`、`userapp_builder/recovery.rs:63-96`。

接到 shutdown 广播后立即关闭 store。HTTP server 未在此处 join；恢复扫描器是独立 spawn 的循环，没有接入 shutdown 信号与 join。已进入 runtime 写入的协调任务也没有在关闭 store 前等待收束。

触发：业务已受理并在创建/停止容器，进程收到 SIGTERM，数据库队列关闭，稍后业务要提交终态/持久化恢复状态却无法写入。排空数据库队列不等于排空业务任务。PG 路径同样受新接线影响。

修复：停止接收新操作，停止恢复/清理生产者，追踪并等待在途协调任务在有界关机预算内收束，再关数据库。预算耗尽时明确记录未完成，不强行清理不确定资源或报告全成功。

回归：用屏障暂停 runtime 写入，发 SIGTERM，再释放屏障；确认数据库关闭前终态或恢复保护能提交、没有新恢复任务接单。分别覆盖 Turso 与 PG。

## R03 / P2：并发 shutdown 可提前成功，线程 panic 被吞掉

源码：`turso/mod.rs:181-205`。

第一次 shutdown take handle 后等待线程；第二次看见 None 就立即 Ok，未等第一次完成。join.join() 的结果被 drop，worker panic 也可报告 Ok。第一次 shutdown future 被取消时，后续调用也无法观察原关闭任务结果。

修复：共享 Running/Closing/Closed 关闭状态与完成通知，所有调用等待同一个最终结果；传播 join panic，关闭任务不依赖首个调用方 future 存活。

回归：有在途任务时并发两个 shutdown；两者均不得提前成功；取消首个等待者后第二个仍能得到真实结果；worker 故障不得返回成功。

## R04 / P2：错误路径仍依赖延迟回滚，队列满也无有界失败

源码：`turso/exec.rs:110-124, 198-208`；`turso/mod.rs:151-177, 209-217`。

Tx 没有显式 rollback；业务函数 `?` 返回后通过 Turso 的 dangling_tx 延迟到下一次语句回滚。已核对本机锁定依赖源码，确实有这种机制，故不声称“必然会提交半事务”。但这仍不符合已确认的“本方法完成回滚后才回包；回滚失败隔离连接”约束：回滚故障会归到下一请求，worker 没有后端不可写状态。

另：队列注释声称满时明确报错，实际是 send().await，满时无限等容量。队列有界不意味着等待时间有界。

修复：统一完整事务执行器负责 begin/body/commit 或 rollback，保留主错误与 rollback 错误，状态不确定时拒绝后续写；队列选择 try_send 明确拒绝或有预算的容量等待，不能给已入队且可能提交的任务错误地承诺“未执行”。

回归：故障发生后无需下一查询就验证回滚已完成；注入 rollback 失败后下一业务不执行；填满队列验证有界失败；提交回包丢失按同 request_id 查询结果。

## R05 / P2：遗漏旧库目录保护，可能静默产生第二份应用身份

源码：`config/userapp_storage.rs:114-126`、`turso/mod.rs:263-287`、`turso/migrations.rs:20`。

方案明确要求：新库不存在而目录存在旧 userapp.sqlite3 时拒绝启动，使用独立目录。当前直接创建新 userapp.turso.db，未检查旧库；verification 的环境记录也写了“旧库保留，新库独立生成”。

无需历史迁移，但不能自动删数据，也不应静默把同一运行环境中的既有操作/资源身份当成不存在。

修复：在独占锁内、创建新数据库之前检查默认旧库及必要残留标识；新库不存在且旧库存在时明确拒绝并指引使用独立目录。不要扩展为数据迁移。

回归：仅旧库→拒绝且新库/旁文件未生成；空目录→成功；已有正常新库→可按确认策略打开；旧文件内容不变。

## R06 / P2：配套仓库未完整同步，报告超出实际实现

配套源码：`/Users/soddy/Documents/git-workspace/build-agent-docker/build_config/rcoder/start-services.sh:4-12` 仍判断 backend=sqlite、读取 RCODER_USERAPP_SQLITE_PATH。Turso 模式跳过目录 fail-fast 预检。配套 `docker/SQLITE.md` 也仍是旧文档。

Turso verification T3 声称两仓启动脚本/文档已同步，但实际不符。该仓库 Compose 改动和 STS 改动尚未提交，RCoder commit 不能包含这些外部文件。

修复：按 plan 的两仓清单逐项同步，检查隐藏 env 示例/volume overlay/脚本。分别记录两个仓库版本与未提交差异；只提交对应范围，不把其他工作一起暂存。

验收：两套生产 Compose 与 RCoder Compose 的配置解析；启动脚本在不可写 Turso 目录时提前失败；构建镜像内实际脚本/后端身份核验。

## R07 / 验收缺口：测试通过的范围被扩大解释

本轮独立执行：

```bash
cargo nextest run -p rcoder-storage --features userapp-turso --no-fail-fast -E 'test(turso)'
```

退出码 0；24 passed，71 因筛选未运行。仅证明这些组件用例通过。本轮未重跑 Compose、真实 PG、remote K8s，也未部署/发布。

现有取消测试 `turso/mod.rs:1015-1076` 取消的是 sleep 任务，后面另开事务测试 dangling rollback；不能替代“实际写事务被调用方取消/初始化取消/关机竞态”证据。

交付 verification 明确记录完整 Compose 有 8 个失败，tasks 却勾选故障矩阵和完整验收。app-cli config_hash mismatch 的归因还写着“疑为”，无同条件基线复跑证据，不能只因不在 diff 就归结为确认既有问题。M4 进程锁冲突响应缺少 blocker.scope 也不能直接通过修改测试预期解决；应对照 dev/prod 查询与错误契约确认是否允许该路径省略 blocker。

修复：将执行完成与验收通过分开；给 8 个失败逐项明确根因、基线对照或修复报告，最终补完整 Compose。K8s 全套通过可作为 PG/K8s 证据，不能覆盖 Turso worker 的错误路径。

## RBD/STS 配套方案的状态

本轮抽查到桥接 workload selector、STS 0 副本初态、独立 RWO claim 与模板 emptyDir/模板 checksum 已实现；没有据此宣称迁移成功。配套变更仍未提交，`specs/rcoder-local-cache-per-replica-rbd/verification.md` 明确真实安装、迁移/回滚、CSI 调度和业务门禁未运行。普通 remote-k8s 全套通过不能替代这份 chart 的 Deployment→StatefulSet 演练。

## 实施顺序

1. R01 生命周期与锁所有权，R02 业务关机顺序。
2. R03 共享关闭结果，R04 显式事务清理与队列预算。
3. R05 旧目录保护，R06 两仓配置配套。
4. 补对应反例、更新 R07 证据，再完整 Compose；共享关机影响 PG，需追加真实 PG/K8s 针对性验证。

保留用户确认的 PG/Kubernetes 限制，不新增 Compose PG 或多副本能力；不以修复为由清理实际应用记录或租约。

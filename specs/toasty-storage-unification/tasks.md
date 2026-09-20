# Toasty 迁移任务与验收

UserApp、Preview、ProjectStore 与 Activity 的 Toasty 实现及 SQLx 移除已落入当前工作树。真实 PG 的 ProjectStore/适配器回归 62/62、扩展 PG E2E 的 23 项必需断言及三份 Compose Turso 重建的 21 项断言已有通过记录；完整业务 Compose、K8s 与发布尚未完成，因此当前不能声明可发布。本轮已将有明确组件证据的项目更新，逐项依据及未完成子项见 [完成度审计](completion-audit-2026-09-20.md)。下面未勾选项目仍需逐项核对完整证据，不以源码存在替代实测。详见 [验证记录](verification.md)、[库表设计](schema-design.md) 与 [SQL 基线及测试库重置](schema-baseline-and-test-reset.md)。

## T0：正式版本可行性门槛

- [ ] 在独立临时验证项目解析 Toasty 0.10.0 / Turso 0.7.2，记录版本、发布源码 SHA、Cargo tree；无需修改生产依赖即可先做探针。
- [ ] 编译默认本地、PG、两后端组合；核对 MSRV、TLS、驱动默认 features、Docker/远端构建工具链。
- [ ] 用真实文件 Turso 和独立测试 PG 验证新 schema 的模型映射、复合键/唯一约束、JSON/时间/NULL/整数范围、参数化 SQL；故意写入非法行必须被拒绝，不能只验证 CREATE 成功。
- [x] CAS 反例：两个独立 PG store 争同一 revision 恰好一方成功；零行与一行条件写可区分；Turso 相同业务断言通过。 证据及范围见 [2026-09-20 完成度审计](completion-audit-2026-09-20.md)。
- [x] 完整业务事务反例：写 operation 后注入失败，request/input/slot 均不残留；取消受理方后仍可查到唯一、完整结果。 证据及范围见 [2026-09-20 完成度审计](completion-audit-2026-09-20.md)。
- [x] BEGIN/COMMIT/ROLLBACK 各阶段故障注入，确认未知提交不假失败解锁、rollback 失败连接不被复用。
- [x] 实测 ManagedDriver 包装：阻塞写期间 shutdown 不提前成功；并发 shutdown 共享结果；取消首个 waiter 不取消关闭；锁在最后连接/runtime 退出前不释放。 证据及范围见 [2026-09-20 完成度审计](completion-audit-2026-09-20.md)。
- [x] 双副本 PG 迁移同时启动；checksum 改动/未来版本拒绝；DDL 中途失败不留半迁移。 证据及范围见 [2026-09-20 完成度审计](completion-audit-2026-09-20.md)。
- [x] PG session advisory lock：正常退出、超时、断网、取消后没有带锁连接回到业务池；另一实例按真实数据库锁状态获主。 证据及范围见 [2026-09-20 完成度审计](completion-audit-2026-09-20.md)。
- [x] Turso PRAGMA 回读、提交后进程重启恢复；不把正常 reopen 写成断电持久性验证。旧版本开发 DB 拒绝原地降级打开。
- [ ] 输出 T0 通过/失败矩阵与后端专用代码清单。关闭/隔离接口不能满足时停止批量迁移，记录确切阻断项。

## T1–T4：实现

- [x] 建立内部 Toasty 基础设施，保持业务 trait 不暴露 ORM。 证据及范围见 [2026-09-20 完成度审计](completion-audit-2026-09-20.md)。
- [x] UserApp 共用完整事务流程；穷举全部 trait 方法及每种 scope，删除重复算法。 证据及范围见 [2026-09-20 完成度审计](completion-audit-2026-09-20.md)。
- [ ] 按新设计完成 UserApp 九张生命周期表及 CR10 三张配置表、统一请求命名空间、当前三槽位、完整身份复合 FK，停止存储整条重复 record JSON。
- [x] 迁移 Preview，补活跃端口唯一索引与 PG 短事务分配锁；验证同 key 首创、不同 key 抢端口、旧 instance/operation/revision 拒绝。 证据及范围见 [2026-09-20 完成度审计](completion-audit-2026-09-20.md)。
- [x] 迁移 ProjectStore repo/load/writer/sync/leader，补 container generation 与 write-behind CAS，删除 sessions 冗余容器列；保留 tombstone、批处理、回源与 flush 语义。 证据及范围见 [2026-09-20 完成度审计](completion-audit-2026-09-20.md)。
- [x] 调整 ActivityRow/ActivityPersistence/采集与代理传播，以 lifecycle 隔离并单调写活动时间；取消持久 stopped/wake_blocked 的第二权威。 证据及范围见 [2026-09-20 完成度审计](completion-audit-2026-09-20.md)。
- [ ] 删除 publish_tasks、旧 userapp_metadata 表/导入与确认无消费者的接口；同步配置工厂、AppState、shutdown、PG 测试工具。
- [x] 15 个旧 SQL 收敛为 4 个最终初始化文件，统一 rcoder_schema_migrations；修正 include_str!/迁移目录/旧表名在源码、测试与部署脚本中的全部消费者。 证据及范围见 [2026-09-20 完成度审计](completion-audit-2026-09-20.md)。
- [ ] 执行 schema-design.md 的 S01–S12 反例矩阵，逐后端记录结果；与 T0 探针复用有效场景，不用重复运行掩盖缺失断言。
- [x] 删除所有 SQLx 依赖/宏/运行代码，删除不再需要的直接 turso 预发布依赖；根依赖图无 SQLx，引擎只解析预期正式版。2026-09-19 metadata 已核验，故障测试迁移后探针 72/72；不代表全仓验收。
- [ ] 核查 build-agent-docker 及 tests-e2e 的配置/工具引用。保留 pg/kubernetes 限制、现有内存模式和部署范围。

## T5：验证与交付

- [x] 聚焦 nextest：存储与消费方，默认、PG 与全 features；保留实际测试数、命令和退出码。 证据及范围见 [2026-09-20 完成度审计](completion-audit-2026-09-20.md)。
- [x] `cargo fmt --all -- --check`、受影响 feature 的 clippy。 基线 `e3959112` 已通过，见 verification 的集中回归记录；后续修改仍须补受影响检查。
- [x] `cargo nextest run --workspace --no-fail-fast --all-features`；额外检查默认 features。Cargo 任务串行，不共用 target 并发运行。 基线 `e3959112` 已通过，见 verification 的集中回归记录；后续修改仍须补受影响检查。
- [x] app-cli 只有在共享依赖或代码受影响时执行独立项目检查，根测试不能代替它。 基线 `e3959112` 已通过，见 verification 的集中回归记录；后续修改仍须补受影响检查。
- [ ] 本地 Compose：完整 test-e2e，含 UserApp 开发/生产、并发作用域、失败重试、重启恢复与文件代理。
- [ ] 对已授权的个人测试 PG 执行一次性重建：读取私有配置核验目标、停止旧写入者、记录并处理旧运行资源归属、重建目标 database、启动新双副本。记录非敏感对象清单与结果，不在报告写凭据。
- [ ] remote K8s：重建完成后 verify 部署当前快照，再对同一快照运行 UserApp、Chat 和受影响 Preview/Gateway 场景；补 PG 双副本争用、数据库断连恢复、关机期间写入等缺失覆盖。
- [ ] 验证空库并发初始化及持久化重启；checksum/未来版本/关键约束缺失反例在独立测试数据库进行，不篡改验收用库后不还原。后续回归保留正常数据，不每轮清库。
- [ ] 报告区分实现、组件测试、真实 PG、Compose、K8s 与发布状态；未执行或环境阻塞不记通过。
- [ ] 已批准的首次基线重建与测试修复分开记录；不得反复清库/清锁/降低断言掩盖错误，不改写历史报告。保留未提交的其他开发改动，按任务文件或改动块提交。

逐项测试名及未覆盖分支见 [T0 / S01–S12 证据映射](acceptance-matrix-2026-09-20.md)。新增两项勾选仅依据既有边界隔离测试及 CR06 进程重启证据，不含断电保证。

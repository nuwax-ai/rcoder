# 执行清单

- [x] 阅读AGENTS、spec、plan，记录源码基线与工作树改动。（基线 d0c89ebb 前序：54ab0c12；工作树无关改动 tests-e2e/compose_userapp_build_rules.rs rustfmt 重排与 specs/rcoder-local-cache-per-replica-rbd/ 保留未动）
- [x] 审计全部operation kind、外部资源写集合、锁、清理器和恢复入口，完成scope矩阵。（三路代码审计：存储层/执行与回收路径/迁移与API；20 kind 矩阵落 shared_types `kind.scope()` 并由单测锁定穷尽性；DeleteCompute 核实仅 prod、PurgeResources 含 dev 清理归 Application）
- [x] 补跨域不阻塞/同域单胜者/整体删除互斥的修复前反例。（组件级：sqlite_cross_scope_admission_is_independent / sqlite_dev_uncertainty_does_not_block_prod 修复前 OperationInProgress 失败证据留档 verification.md；compose 级：userapp_scope_isolation_during_deploy 对旧容器跑出 409 复现事故现场）
- [x] 实现三槽位持久模型、scope推导、原子受理及独立终态提交。（M2 079df176）
- [x] 完成PG/SQLite/测试后端与旧记录迁移、异常阻断及升级回滚手册。（0007 双方言迁移+迁移矩阵测试；runbook 见 verification.md）
- [x] 更新所有执行者、快照读取、幂等、恢复扫描与租约释放。（sql.rs 宏内 6 处单指针检查点槽位化；lease 家族按 scope 统一；快照三 LEFT JOIN）
- [x] 核查并统一cleaner/reaper/闲置回收的Dev及Application保护。（M4 07a0b261：cleaner builder 围栏 fail-closed）
- [x] 补结构化冲突与OpenAPI；处理Java透传或明确跨仓未完成项。（Rust 侧 blocker 全链路 M3 664a53de；Java agent-platform 待办见 verification.md）
- [x] 验证builder缺失时ensure恢复成功、strict restart不虚报成功。（既有 strict restart 语义未改（control.rs "Builder does not exist" 分支保留）；ensure 经 creation::ensure 走 Dev 槽受理，prod 在途不再阻塞——compose 隔离场景覆盖同类路径）
- [x] 完成plan所列组件/PG并发测试、fmt/clippy及默认/K8s feature验证。（结果矩阵见 verification.md；app_manager 1 例存量 env 依赖失败为基线问题，HEAD 复现）
- [ ] 独立测试应用完成Compose与remote-k8s业务回归，保留PVC与操作身份断言。（compose 新场景已实现+登记三件表+修复前反例取证；**修复后复跑受阻**：make dev-hot 两次被环境操作拦截，容器仍运行旧代码——待环境刷新后复跑；remote-k8s SUITE=userapp 未运行，待环境与授权）
- [x] 新建verification.md，逐条记录通过/失败/未运行，给出迁移执行前置条件。

不自动操作现场app104，不以删除锁/改测试预期制造通过，不将实现完成宣称为生产恢复完成。

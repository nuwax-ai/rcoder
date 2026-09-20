# 当前开发验证记录

## 范围

未提交，尚未完成整体功能；没有修改或部署集群。

已实现基础：增量 v2 迁移、控制受理/执行者认领、Stop 优先级、原始请求幂等、dev/prod 分离、旧成功提交保护、新业务受理保护。按用户反馈移除独立请求映射表。按最终反馈，停止期间的新 Restart 立即冲突，不保留队列；停止完成后显式重试可受理。旧执行者仍可保存已发出写入的诊断证据，不能提交成功或开启重试。

## 验证

- 简化前 rcoder-storage 全 features：168 passed，13 skipped，exit 0。不是最终变更的全量证明。
- 最新聚焦：`cargo nextest run -p rcoder-storage --all-features --no-fail-fast -E 'test(compute_) or test(baseline_is_repeatable)'`，7 passed，177 filtered，exit 0；run 48d08f04-c28f-49ac-9e86-7887e697fb9e。
- `git diff --check` exit 0；已执行 cargo fmt。
- 停止完成后的重新受理测试以数据库 fixture 表示协调器已确认退出，仅验证存储时序，不代表真实 Pod 已停止。

## 仍需实施和验证

- 控制协调器、执行者取消/收束、运行时每个写边界授权及在途写回执。
- Stop 完成提交及后台恢复；普通 Start 在停止期间的冲突响应、停止后的显式唤醒。
- dev/prod 物理操作、RBD 排他、旧资源自动登记、202/查询/恢复接口和 OpenAPI。
- 默认 features、最终全 features、Clippy、独立连接 PG 并发、Compose、K8s。
- Java 联调和 app 129 的真实恢复。当前 HTTP 仍走原链路，不能宣布用户故障已修复。

## 本轮：执行边界与独立 PG 验证

实现 `check_business_execution`，核对普通操作的 lifecycle、operation、executor、request fingerprint、scope slot 和控制意图。已接入 builder 创建/计算控制，以及 app_manager 创建、更新、启停、恢复、流量唤醒、策略、部分存储及数据库管理路径的写前边界。读取私有执行输入、绑定租约同样检查旧操作是否已被中断。检查并不取消已经发出的网络请求，也不证明远端写结束。

Running 阶段仍允许持久化已发出请求的证据；禁止旧操作提交 Succeeded、进入 WaitingRetry 或重新认领。避免阻断证据记录导致恢复失去依据。

实际验证（均 exit 0）：

- 三 crate 全 features：`cargo nextest run -p rcoder-storage -p app_manager -p rcoder --all-features --no-fail-fast`，728 passed，14 skipped；run `dbacd76f-ca46-4ad8-8c16-8069e30e471c`。
- 该轮之后补入 wake/policy/storage 写前检查，追加 `cargo nextest run -p app_manager --all-features --no-fail-fast`：215 passed，0 skipped；run `1fda4493-a367-448d-aed2-7888ef0dc7d9`。
- 真实调用链反例 `priority_stop_after_capture_prevents_old_builder_mutation`：在 runtime 捕获身份处暂停、持久化 Stop、恢复旧执行，断言 apply 调用次数 0、原只读租约释放 1 次、新 Stop 仍 Pending；run `9ea51c5a-07ed-4d79-85ab-d0176c137355`。
- 一次性本地 PostgreSQL 17，两独立 store/连接，12 轮 Stop/Restart 竞争及执行者抢占：`cargo nextest run -p rcoder-storage --all-features --no-fail-fast --run-ignored only -E 'test(compute_pg_independent)'`，1 passed；run `e54552ef-9a35-4cec-b27e-465d05da832a`。专属测试容器已停止并自动删除，未使用现有业务数据库。

这不是功能验收：高优先级控制的执行器、独立租约证据、终态提交及恢复入口尚未实现，HTTP 未接入新受理方法。热点部署内部、运行时复合写的每个阶段仍需核查；写前读取不能代替原子远端身份前置条件。不能把现有操作租约直接绑定到新控制表：v1 lease 外键引用普通 operations，需要明确实现独立控制回执或统一操作记录，不能绕过外键或遗漏扫描回收。

- 严格 Clippy：`cargo clippy -p rcoder-storage -p app_manager -p rcoder --all-targets --all-features -- -D warnings` 首轮 exit 101（新增 compute.rs 的 collapsible_if、新增 PG 测试的 unnecessary_unwrap）；修正后 exit 0。
- 当前 fmt 与 `git diff --check` 通过。没有改 app-cli，未运行其独立测试；未运行 Compose/K8s E2E，也未提交或发布。
- 默认 features 聚焦：`cargo nextest run -p rcoder-storage -p app_manager -p rcoder --no-fail-fast -E 'test(compute_) or test(priority_stop_after_capture)'`，10 passed，694 filtered，exit 0；run `ce24cfdb-5f59-4dc1-9cc2-daa6275816e4`。macOS 链接器报告既有体积相关的 unwind section 警告，测试执行成功；未将此警告当作功能验收。


## 2026-09-20：控制执行账本与独立回执回收

实现：

- 控制行新增独立 lease/checkpoint 与终态约束，v1 baseline 不变；v2 尚在本工作树开发、未发布。没有借用普通操作 lease 表或取消其外键。
- 回执绑定不可覆盖；阶段按收束→停止→停止确认→启动→验证推进；Stop 不允许进入启动阶段。
- 终态完成必须通过原 executor/代次/revision，旧业务与被替代控制仍未收束时禁止进入物理变更阶段。
- 已开始变更的失败保留 RecoveryRequired，不能以 Failed 释放未知写。Superseded 原 executor 只能确认收束，不能提交成功或清理新控制。
- 后台扫描仅释放 Succeeded/Failed 的精确回执；释放失败保持账本，Superseded 不等于安全可回收。

验证记录：

- 第一轮新增反例 7/9，2 个失败定位为 Toasty 原生 SQL 绑定 None 时缺少显式类型；已改用 bind_typed，不调整断言。
- 聚焦 `cargo nextest run -p rcoder-storage --all-features --no-fail-fast -E 'test(compute_)'`：10 passed，exit 0；run `adc1e503-beae-4660-8387-477d17f22599`。
- 三 crate 全 features `cargo nextest run -p rcoder-storage -p rcoder -p app_manager --all-features --no-fail-fast`：733 passed、15 skipped，exit 0；run `b62c1ff7-a956-4e9b-9fb6-db6f53e24283`。包含控制终态扫描的真实 Turso/文件回执反例。
- 随后补了持久回执解码时的 scope/owner 校验。独立临时 PostgreSQL 17：`cargo nextest run -p rcoder-storage --all-features --no-fail-fast --run-ignored only -E 'test(compute_pg_)'`：2 passed，exit 0；run `5cb2f626-2c70-4d24-ab1d-0a25fbec8cd2`。覆盖 v1 旧数据/账本不变、重复升级，以及 12 轮独立连接 Stop/Restart 竞争、终态 CAS 和回执清理。仅连接本轮专属测试容器。

局限：控制进度/收束接口是内部存储契约，运行时证据仍需协调器生成与核验；没有新增接收任意 JSON 并授权清锁的 HTTP 入口。本轮未改变线上路由、未部署这些源码，尚不能用新增方法完成 app129 恢复。没有运行 Compose/K8s E2E，没有修改 app-cli，没有自动提交/发布。

- 最终默认 features 聚焦：`cargo nextest run -p rcoder-storage -p rcoder -p app_manager --no-fail-fast -E 'test(compute_) or test(priority_stop_after_capture)'`，15 passed，exit 0；run `2b484712-efc3-4ce8-a1b7-1e92bc1e7d18`。694 个未选中用例不能算本轮默认全量通过。
- 严格 Clippy：`cargo clippy -p rcoder-storage -p app_manager -p rcoder --all-targets --all-features -- -D warnings`，exit 0。
- `cargo fmt --all -- --check` 与 `git diff --check`，exit 0。
- 专属 PostgreSQL 测试容器已停止并自动移除。macOS 测试链接仍有 unwind section 体积警告，执行结果正常。


## 2026-09-20：未登记 dev owner 与未执行业务收束

源码基线 `447247b5` + 本工作树，保留此前所有未提交开发。未提交、未发布、未修改 .18 集群。

- file-server / rcoder-storage / runtime-state-layout 初轮 all-features：539 passed、15 skipped，exit 0，run `f3d2e25a-3902-42ca-bbb2-dd9628b4340f`。该轮之后收窄来源记录用途，不能把初轮视作最终目录行为验证。
- 最终文件服务 all-features：`cargo nextest run -p file-server -p file-server-userapp -p runtime-state-layout --all-features --no-fail-fast`：450 passed、0 skipped，exit 0，run `98365970-9ca6-468f-9db4-00f57ee8a4d3`。
- 存储/owner 聚焦：`cargo nextest run -p rcoder-storage -p file-server -p runtime-state-layout --all-features --no-fail-fast -E 'test(compute_) or test(external_stop_tests) or test(project_origin_tests)'`：20 passed，exit 0，run `2b89a03f-f93c-40fe-97e3-a71faddb1e79`。其后 owner 工作区内旧目录支持由最终450例覆盖。
- 默认 features 聚焦：四 crate（上述文件服务三 crate + rcoder-storage），筛选 external_stop_tests / identity_uses_canonical / project_origin_tests / compute_：10 passed，exit 0，run `a8ce5613-28c7-4823-be84-69ec6357ae4c`。rcoder-storage 默认不启用 Turso/PG，因此不能称默认模式执行了存储测试；存储见 all-features 与独立 PG。
- 独立临时 PostgreSQL 17、两个 store 各单连接：`cargo nextest run -p rcoder-storage --all-features --no-fail-fast --run-ignored only -E 'test(compute_pg_independent)'`：1 passed，内部12轮竞争，exit 0，run `80b69b38-738c-47f5-8948-7f71f41d2ab6`。新增断言验证控制认领与取消 Pending 同事务对另一连接可见。临时测试容器已停止并确认自动删除；未用业务 PG。
- app-cli 初轮构建来源/目录身份聚焦：3 passed，exit 0，run `2c682936-5c30-4d36-b945-933adce36f28`；最终检查追加在下方。

实现边界：没有把未知运行时写标成失败，没有让新的 Start/Restart 在 Stop 内部排队。控制执行器与自动登记恢复尚未完成。Compose、K8s、Windows/Linux 实机及 app129 恢复未验证。新增来源记录为本地构建元数据，不用它重定位已有 owner 的状态根。

- app-cli 最终独立 all-features：`cargo nextest run --manifest-path crates/app-cli/Cargo.toml --all-features --no-fail-fast`，264 passed、1 skipped，exit 0，run `79765903-36bd-42e8-a270-b864dc037db3`。包含真实二进制 owner 凭据发布和子进程树停止测试；不是 Windows/Linux 实机或 Compose 验收。
- 根受影响四 crate 严格 Clippy（storage、file-server、file-server-userapp、runtime-state-layout）all-targets/all-features，exit 0。其后恢复 Start/Deploy 原有目录核验（来源记录只用于 Stop），最终聚焦追加在下方。

- 最终 owner 聚焦：`cargo nextest run -p file-server --all-features --no-fail-fast -E 'test(external_stop_tests) or test(r09_tests)'`，10 passed，exit 0，run `032218df-0b53-4d8a-bf6b-67a8f62c5c60`。核验旧手动目录、认证、manager 重建、目录越界及 Start 核验不放宽。
- app-cli 严格 Clippy：`cargo clippy --manifest-path crates/app-cli/Cargo.toml --all-targets -- -D warnings`，exit 0。
- 最终 `cargo fmt --all -- --check`、app-cli 独立 fmt check、`git diff --check`，分别 exit 0。中途 fmt check 发现新增测试排版差异，格式化后通过。

## 多进程交替控制 E2E（2026-09-20）

新增一个 `compose_userapp_dev::userapp_manual_owner_multi_process_control`，复用手动 owner 脚本及轻量 HTTP fixture，纳入 suite_cases、acceptance_steps、report_identities 的严格登记。

顺序：手动 serve → RCoder Stop → 新 HTTP 客户端重复 Stop → 新 app-cli run 转交 Start，连续两轮，最终 Stop。断言实际业务响应、同 owner runtime_instance_id、临时 CLI 退出后仅剩原 owner、容器及文件保留。仅删除本次捕获 ID 的临时 builder，保留工作区卷。

- 命令：`CARGO_BUILD_JOBS=2 E2E_SUITE=compose_userapp_dev E2E_FILTER=userapp_manual_owner_multi_process_control make test-e2e`。
- 正式冻结源码轮退出 0：[0631189feae54855a848bf8c8f6a52ba](../../tests-e2e/reports/0631189feae54855a848bf8c8f6a52ba/summary.json)，1 个场景通过，无缺失断言、无漂移。
- 首次登记未齐退出 2；补正登记期间的一轮行为通过但源码漂移，仍记失败，不替代正式结果。
- `cargo clippy -p rcoder-e2e --test compose_userapp_dev`、根 fmt check、Python 语法检查与 diff check 均退出 0。
- 只新增一个集成测试，不增加 helper 单测。未覆盖多 RCoder 副本并发、跨目录切换、K8s 控制器或未知迁移恢复；这些未完成项保持不变。

## 2026-09-20：生产控制链接入（未部署）

基线 HEAD `447247b5` 加当前未提交工作树。按用户最新要求没有新增测试代码，没有执行 nextest、Compose 或 K8s 部署。已接入生产 HTTP 路由、Pending 发现、控制执行器、原操作收束、停止意图与受限恢复入口；实现边界见 tasks.md 最新一节。

- 最终 `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-compute-check.log`。中间编译发现并修复枚举名、错误映射、限定路径与语法问题。
- `cargo fmt --all`、`git diff --check` 退出 0。
- 仅证明生产代码的全 features 编译；未证明默认 feature、测试目标编译、Clippy、运行时正确性和完整功能验收。既有 E2E 通过记录不覆盖本轮控制执行器。
- 未提交、未推送、未修改集群。

## 已确认计算操作恢复（续）

新增生产逻辑：确认完成边界的原操作终态补交、后台发现、终态回执直接清理、panic 阶段记录及 Restart 登记前的执行身份复核。详见 tasks.md 最新章节。

- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-compute-recovery-check.log`。首轮限定路径 lint 失败，修正后通过。
- `cargo fmt --all`、`git diff --check` 退出 0。
- 按用户要求未新增测试、未跑集成测试；默认 features/测试目标/实际运行验证未执行。没有提交、推送或操作集群。

## 缺失生命周期发现恢复（续）

本轮仅新增生产逻辑与文档，没有新增或执行测试用例。实现范围与剩余项见 tasks.md 最新章节。

- 最终 `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-discovery-check.log`。集中编译发现 AppServiceTrait 缺少转发声明，补齐后通过。
- `cargo fmt --all`、`git diff --check` 退出 0。
- 未验证真实数据库竞态、Docker/K8s 部署、默认 features 和测试目标编译；未提交、推送或修改集群。不能据本轮编译结果宣布 T5 整体验收完成。

## app-cli journal 边界修正（续）

- `cargo fmt --manifest-path crates/app-cli/Cargo.toml` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check --manifest-path crates/app-cli/Cargo.toml --all-features` 退出 0；日志 `/tmp/rcoder-app-cli-recovery-check.log`。
- 未新增、未执行测试；未运行容器或三平台实测。此次修复阻止新的错误 Switching 记录，不证明历史不确定记录已恢复。

## builtin 启动期间停止（续）

- app-cli 独立 fmt、`CARGO_BUILD_JOBS=2 cargo check --manifest-path crates/app-cli/Cargo.toml --all-features`、`git diff --check` 均退出 0。
- 编译日志 `/tmp/rcoder-app-cli-startup-stop-check.log`。
- 未新增/运行测试，未验证真实进程关闭或迁移中断；代码实现不等于真实部署验收。没有提交或发布。

## supervisord 启动期协作取消（续）

- app-cli 独立 fmt、`CARGO_BUILD_JOBS=2 cargo check --manifest-path crates/app-cli/Cargo.toml --all-features`、`git diff --check` 退出 0。
- 编译日志 `/tmp/rcoder-supervisord-cancel-check.log`；最后一轮无编译警告。
- 未新增或执行测试，未运行 supervisord 容器、三平台或数据库故障注入。没有提交、推送、发布或操作集群。

## owner 执行项目与 prod Ready 身份核验（续）

- 根 workspace 与 app-cli 独立 fmt、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p file-server --all-features` 最终退出 0；初次因两处冗余 Path 限定被 lint 拒绝，修正后通过。日志 `/tmp/rcoder-owner-origin-root-check.log`。
- `CARGO_BUILD_JOBS=2 cargo check --manifest-path crates/app-cli/Cargo.toml --all-features` 退出 0；日志 `/tmp/rcoder-owner-origin-cli-check.log`。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0；日志 `/tmp/rcoder-owner-origin-full-check.log`。
- 未新增或执行测试，未运行 Compose、K8s、三平台，未提交或发布。编译通过只证明当前生产目标构建通过；历史无来源目录与其他剩余实现见 tasks.md。

## 热部署凭据应用顺序（续）

- 根 workspace 和 app-cli 独立 fmt、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-hot-pg-check.log`。
- `CARGO_BUILD_JOBS=2 cargo check --manifest-path crates/app-cli/Cargo.toml --all-features` 退出 0，日志 `/tmp/rcoder-hot-pg-cli-check.log`。
- 按本轮要求未新增、未执行测试。未运行数据库改密、热部署或进程重启实测，未提交、推送和发布。持久凭据恢复与 hot_execution 完整恢复仍未完成。

## 私有运行凭据恢复（续）

- `cargo fmt --manifest-path crates/app-cli/Cargo.toml`、`git diff --check` 退出 0。
- 最终 `CARGO_BUILD_JOBS=2 cargo check --manifest-path crates/app-cli/Cargo.toml --all-features` 退出 0；日志 `/tmp/rcoder-private-credentials-check.log`。
- 未新增或运行测试，未执行三平台文件权限/进程重启实测，未运行 Compose/K8s；未提交、推送或发布。平台直接改密到显式启动的配置注入链仍需核对，不能凭 app-cli 私有存储宣布全链闭环。

## 凭据不落盘约定修正（续）

- 上一轮私有凭据文件功能已撤回；仅进行源码与编译，没有创建真实运行凭据文件。
- `cargo fmt --manifest-path crates/app-cli/Cargo.toml`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check --manifest-path crates/app-cli/Cargo.toml --all-features` 退出 0，日志 `/tmp/rcoder-credential-recovery-contract-check.log`。
- 未新增或执行测试，未提交、发布或操作部署环境。重新提供凭据的完整恢复入口仍未完成。

## 缺失 prod 计算资源的 Stop（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-absent-stop-check.log`。
- `CARGO_BUILD_JOBS=2 cargo check -p docker_manager` 默认 features 退出 0，日志 `/tmp/rcoder-absent-stop-default-check.log`。
- 未新增或运行测试，未实际创建/删除 Docker 或 K8s 资源，未提交、推送或发布。缺失证明必须由真实环境后续验收，编译不能替代并发和残留资源验证。

## K8s Stop 原子回执恢复（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 最终退出 0，日志 `/tmp/rcoder-stop-receipt-check.log`。首轮发现存储模块两处冗余限定及 String/&str 错误适配，修正后通过。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder` 默认 features 退出 0，日志 `/tmp/rcoder-stop-receipt-default-check.log`。
- 未新增或执行测试，未对集群发送缩容或恢复请求；未提交、推送、发布。响应丢失、多副本 CAS 与 Pod 退出时序尚待后续实际验收。

## K8s Restart 启动回执恢复（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-start-receipt-check.log`。
- 未新增或执行测试，未调用真实 K8s 启动/恢复接口；未提交或发布。本轮未重新编译默认 features，不将此前默认编译结果表述为本轮验证。

## K8s Restart 停止后续行（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- 最终 `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-restart-continuation-check.log`。
- 未新增或运行测试，未操作真实 K8s/Docker 资源，未提交或发布。默认 features、双执行者竞争、停止抢占和崩溃时序未在本轮实测；实现不等于部署验收。

## 计算启动单写边界与卷见证（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- 最终 `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-compute-start-witness-check.log`。
- 未新增或执行测试，未运行实际 PVC 更换、RBD 挂载或中断续行验证；未提交或发布。单写崩溃重试尚未接入，本轮不宣称恢复功能全部完成。

## Starting 条件重试及 superseded 完成边界（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- Starting 重试编译：`CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-compute-start-retry-check.log`。
- 收束逻辑编译：同命令退出 0，日志 `/tmp/rcoder-superseded-boundary-check.log`。
- 按用户要求未新增或执行测试；未做默认 features、Compose、K8s 实测，未提交、推送或发布。编译通过不代表并发时序已经验收。

## Stop 隔离旧 Restart 条件启动（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 最终退出 0，日志 `/tmp/rcoder-compute-start-fence-final-check.log`。
- 未新增或执行测试，未运行 Compose/K8s；实际 API 条件竞争与响应丢失时序尚未验证。未提交、推送或发布。

## 阻塞解除后自动续行（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-compute-drain-resume-check.log`。
- 本轮未新增或运行测试，未验证默认 features、Compose/K8s 扫描时序和双副本竞争；未提交、推送、部署。

## 发现登记卷身份与 v3 证据迁移（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-recovery-volumes-check.log`。
- 未新增或运行测试；v2→v3 的 PG/Turso 实际升级、PVC 换代和停止资源恢复尚未实测。未提交、推送、部署。

## 恢复卷证据的读取与使用前核验（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-recovered-storage-use-check.log`。
- 未新增或运行测试，未执行默认 features、数据库迁移、Compose/K8s 实测。未提交、推送、部署。

## 恢复卷挂载引用核验（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-recovered-mounts-check.log`。
- 未新增或执行测试；当前模板改挂、PVC 替换和控制器缺失场景均未做集群实测。未提交、推送、部署。

## 被替代 Restart 缩容写隔离（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-superseded-stop-fence-check.log`。
- 未新增或执行测试，未操作集群；未提交、推送、部署。

## K8s dev Stop 回执恢复（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-builder-stop-receipt-check.log`。
- 未新增或运行测试；未做实际 StatefulSet 缩容响应丢失、Pod 退出或多副本恢复验证。未提交、推送、部署。

## K8s dev Restart 启动回执恢复（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 最终退出 0，日志 `/tmp/rcoder-builder-start-receipt-final-check.log`。
- 未新增或运行测试；未实测响应丢失、控制器修订换代及多副本终态竞争。未提交、推送、部署。

## dev Restart 停止后续行（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-builder-restart-resume-check.log`。
- 未新增或执行测试；未做实际中断续行、抢占和多副本 CAS 验证。未提交、推送、部署。

## dev Restart 条件写抢占隔离（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- 首次编译发现 unused-qualifications lint，已修正。最终 `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-builder-compute-fence-final-check.log`。
- 未新增或执行测试，未做 K8s 抢占/响应丢失实测；未提交、推送、部署。

## dev Starting 条件重试（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-builder-start-retry-check.log`。
- 未新增或运行测试；实际并发迟到写、状态更新竞争和租约丢失尚未实测。未提交、推送、部署。

## dev Restart 卷见证（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-builder-volume-witness-check.log`。
- 未新增或执行测试；PVC 被替换、停止后续行和条件重试的实际竞争未实测。未提交、推送、部署。

## Docker 启动语义与租约内存储核验（续）

- Docker 仅启动修正：`CARGO_BUILD_JOBS=2 cargo check -p rcoder` 退出 0，日志 `/tmp/rcoder-docker-builder-start-check.log`。
- 增加租约内核验及启动前复核后：`cargo fmt --all`、`git diff --check` 退出 0；`CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-restart-storage-lease-check.log`。
- 未新增或运行测试，未提交、推送或部署。默认 feature 检查早于本轮存储核验时序调整，不冒充最终默认配置检查。
- 编译不证明 Docker daemon 未知写恢复、跨副本时序或 PVC 保护实际通过；这些及历史身份接管、凭据恢复仍需继续完成。

## dev 控制器缺失核验（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0；日志 `/tmp/rcoder-builder-absence-check.log`。
- 未新增或执行测试，未提交或部署。控制器删除后 Pod 延迟退出、孤儿 Pod 与控制器重建竞争尚未实测；残留 Pod 的身份绑定收束未在本轮实现。

## 无写入 Stop 检查点恢复（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-absent-stop-recovery-check.log`。
- 未新增或执行测试，未提交、发布或操作集群。该轮只证明编译通过，未验证检查点持久化与终态提交之间的进程崩溃时序。
- Docker 只读租约核验仍未实现，相关恢复不会得到授权；不宣称两种运行时完整恢复已完成。

## Docker 原租约核验（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder` 退出 0；日志 `/tmp/rcoder-docker-lease-verification-check.log`。本轮未重复全 features 编译。
- 未新增或执行测试；文件替换、持锁进程并发及 daemon 延迟写未实测。未提交、推送或部署。
- 本轮补齐 Unix Docker 的只读原租约核验；未据此宣称 Docker 未知运行时写入恢复完成。

## Docker dev Stop 回执恢复（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-docker-stop-receipt-check.log`。
- 未新增或运行测试、未提交部署。原子文件发布、跨进程崩溃恢复和 Docker 实机行为尚未验证；回执仅覆盖成功响应且确认停止之后的崩溃窗口，不代表全部未知 Docker 写已可恢复。

## Docker prod Stop 回执恢复（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0；日志 `/tmp/rcoder-docker-prod-stop-receipt-check.log`。
- 未新增或执行测试，未提交或部署。prod/dev 回执隔离、进程中断及原容器状态变化尚未实测；Docker Restart 和响应丢失恢复仍未完成。

## Docker Restart/stopped 续行（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0；日志 `/tmp/rcoder-docker-stopped-restart-check.log`。
- 未新增或执行测试、未提交部署。原容器停止后进程中断、恢复 CAS 竞争和 gRPC 就绪尚未实测；Docker starting 未知写恢复仍未完成。

## Docker 启动回执恢复（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0；日志 `/tmp/rcoder-docker-start-receipt-check.log`。随后仅调整回执模块注释和错误文案，diff 检查通过。
- 未新增或执行测试，未提交部署。只读恢复、管理通道就绪和 prod 业务就绪未经实机验证；响应丢失且无回执的 Docker 操作仍保持恢复保护。

## Docker 回执抢占交接（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-docker-superseded-receipt-check.log`。
- 未新增或执行测试、未提交部署。同步 Stop/后台扫描竞争、旧文件锁仍持有和 API 未确认故障尚未实测，不能用编译结果宣称完整抢占验收通过。

## dev 外层就绪等待抢占（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0；日志 `/tmp/rcoder-compute-readiness-cancel-check.log`。
- 未新增或执行测试、未提交部署。250ms 是检查间隔，不是包含数据库延迟的响应 SLA；实际 Stop 抢占时序尚未测试，运行时内部未返回的等待仍未覆盖。

## 空根旧身份恢复（续）

- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0；日志 `/tmp/rcoder-unused-root-recovery-check.log`。
- 未新增或运行测试，未提交/部署/清库。外键顺序、事务竞争、PG/Turso 实际执行和 K8s 恢复尚未验证；有操作历史的新根恢复仍未完成。

## 已确认拒绝 Ensure 的身份恢复（续）

- 静态核对 creation.rs：结构化 RequestRejected 对应 Failed/creation_result/null，其他错误对应 RecoveryRequired。
- `cargo fmt --all`、`git diff --check` 退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-rejected-ensure-identity-check.log`。
- 未新增或执行测试、未提交部署。保留旧请求、外键及 PG/Turso 事务时序未实测；RecoveryRequired 身份冲突恢复仍未完成。

### 2026-09-20：创建确认超时后的迟到成功证据

基线：a571a0ea 加当前累积工作树。本批修改 creation.rs、生命周期 trait 和共用存储实现；未修改运行时创建流程。

- `cargo fmt --all`：退出 0。
- `git diff --check`：退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features`：退出 0，日志 `/tmp/rcoder-late-creation-check.log`。
- 未新增/执行测试、未部署、未提交。编译不证明 PG/Turso 并发和真实 Stop 竞争通过。
- 已保存的迟到成功证据保持 RecoveryRequired，仅交由既有协调器重新核验后收束。迟到错误与迟到成功但管理端点未就绪仍保留未知状态，本批不声称恢复链整体完成。

### 2026-09-20：创建结果先存证、后台就绪核验与 Stop 收束

基线：a571a0ea 加累积工作树。新增 builder_created_observed 检查点；创建完成回执与业务 Ready 分离。恢复只读取得当前地址并探测管理端点，前后核验相同物理 UID，不提前刷新注册缓存。Stop 可依据创建结束回执收束普通操作，恢复成功仍要求 ready evidence。

- `cargo fmt --all`、`git diff --check`：退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features`：最终退出 0，日志 `/tmp/rcoder-late-drain-final-check.log`。
- 中间编译因 `unused-qualifications` 退出 101；删除已有 glob import 下多余路径前缀后通过。早期分阶段检查结果不作为最终基线证据。
- 没有新增或执行测试、没有部署或提交。PG/Turso CAS 竞争、实际 Stop/创建时序仍未运行验证。迟到错误与回执落盘前崩溃仍为后续生产逻辑事项。

### 2026-09-20：迟到拒绝与运行时租约清理

基线 a571a0ea 加当前累积修改。迟到的外层 RequestRejected 通过原操作完整快照 CAS 结束为 Failed；其他错误不允许终结。builder_completion 的租约释放失败改为 ConnectionError，保留原拒绝与清理错误的诊断。

- `cargo fmt --all`、`git diff --check`：退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features`：最终退出 0，日志 `/tmp/rcoder-late-rejection-final-check.log`。
- 首次编译退出 101：RuntimeRequestRejection 缺少事务重试闭包要求的 Clone；补充派生后通过。
- 没有新增、修改或运行测试。builder_completion 的既有清理失败测试仍保留旧 RequestRejected 断言，已在 Tasks 记录后续校正事项；不能宣称现有测试全绿。
- 未提交、部署或操作集群。未知写入与回执落盘前崩溃的恢复继续开发。

### 2026-09-20：dev 孤儿 Pod 停止路径

基线 a571a0ea 加累积工作树。新增仅供 Stop 使用的孤儿 Pod 身份检查点、K8s UID/version 条件删除、只读消失确认与协调器恢复分支。没有创建替代控制器或修改 PVC。

- `cargo fmt --all`、`git diff --check`：退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features`：退出 0，`/tmp/rcoder-orphan-stop-check.log`。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder`：退出 0，`/tmp/rcoder-orphan-stop-default-check.log`。
- 未新增、修改或执行测试；未操作 K8s、未提交或发布。编译只证明默认与 K8s feature 路径可构建，实际孤儿终止/响应丢失/PVC 保留仍需后续集成验证。
- 无身份注解的历史孤儿、Restart 重建及其他原方案剩余项未因此完成。

### 2026-09-20：历史物理绑定接入孤儿停止

基线 a571a0ea 加累积工作树。新增只读候选/带绑定捕获分层，Stop 调用入口已接入。既有绑定成为可选检查点字段，删除前再次按原 UID 核验，不扩大到无登记自动接管。

- `cargo fmt --all`、`git diff --check`：退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features`：最终退出 0，日志 `/tmp/rcoder-bound-orphan-final-check.log`。
- 首轮检查显示 helper 尚未接入的 dead_code 警告；修正 Stop 的实际调用入口后复查通过，不能以首轮编译作为链路完整证据。
- 未新增、修改或运行测试，未操作数据库/集群，未提交。历史无注解 Pod 的真实部署验证未运行。

### 2026-09-20：Docker 创建回执跨进程恢复

基线 a571a0ea 加累积工作树。Docker 创建成功后原子落盘再释放租约；扫描器按原操作读回执、条件释放原租约并保存待就绪证据。未将同名容器、时间到期或锁文件存在作为完成证明。

- `cargo fmt --all`、`git diff --check`：退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features`：退出 0，`/tmp/rcoder-builder-runtime-receipt-check.log`。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder`：退出 0，`/tmp/rcoder-builder-runtime-receipt-default-check.log`。
- 未新增、修改或执行测试，未部署、提交或操作集群。真实进程崩溃/文件租约竞争尚未验证；K8s 回执和运行时回执保存前崩溃仍未实现闭环。

### 2026-09-20：K8s 创建回执与共用校验

基线 a571a0ea 加累积工作树。创建成功后保存 immutable ConfigMap，再释放原租约；跨进程恢复需载荷身份完全匹配并条件释放原 ConfigMap 租约。Docker 回执类型提取到共用运行时模块。

- `cargo fmt --all`、`git diff --check`：退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features`：退出 0，`/tmp/rcoder-k8s-creation-receipt-check.log`。
- `CARGO_BUILD_JOBS=2 cargo check -p docker_manager`：退出 0，`/tmp/docker-manager-creation-receipt-default-check.log`；验证共用类型提取后的默认运行时编译。
- 未新增、修改或运行测试，未部署或操作集群，未提交。ConfigMap 真实权限、网络结果丢失和多副本恢复尚未进行部署验证。
- 回执提交前崩溃与历史回执回收仍未完成，不将本批编译通过称为全需求完成。

### 2026-09-20：Service 写入前置条件

基线 a571a0ea 加累积工作树。K8s builder Service 缺端口修补改为 UID/resourceVersion 条件 Merge；普通和 headless builder Service 复用前核验标签、selector、形态。headless GET 非 404 错误直接传播。

- `cargo fmt --all`、`git diff --check`：退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features`：退出 0，`/tmp/rcoder-builder-service-fencing-check.log`。
- 未新增、修改或运行测试，未部署、提交或访问集群。Service 替换竞争及历史 Service 形态的真实验证未运行。
- 调用链证据：k8s_agent_create::create_agent_container 在 wait_for_pod_ready 后仍写 Service；本轮未将 Ready 作为放行旧租约的依据，回执前崩溃恢复继续开发。

### 2026-09-20：最后 Service 写入的原子完成标记

基线 a571a0ea 加累积工作树。原租约传入 managed builder 创建；最后 Service 写入包含完整操作完成证据，独立 ConfigMap 尚未提交时可读取该证据恢复。普通 agent 创建顺序保持原样。

- `cargo fmt --all`、`git diff --check`：退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features`：最终退出 0，`/tmp/rcoder-service-completion-final-check.log`。
- 未新增、修改或执行测试，未部署、提交。真实 Service create/patch 响应丢失、后续进程崩溃、多副本收束均未运行验证。
- 仅覆盖最后 Service 已提交后的窗口；更早创建阶段、绑定实例恢复和 Docker 的其他崩溃窗口未因此完成。

### 2026-09-20：创建等待安全取消

基线 a571a0ea 加累积工作树。新增不序列化的本地取消信号，由持久 compute 控制明确列出的 interrupted operation 驱动；K8s 新建 builder 在已确认请求间或只读 watch 中响应。实际写请求不被中途丢弃。CreationCancelled 仅在原租约显式释放后可终结为失败，保留取消检查点。

- `cargo fmt --all`、`git diff --check`：退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features`：退出 0，`/tmp/rcoder-create-cancel-final-check.log`。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder`：退出 0，`/tmp/rcoder-create-cancel-default-check.log`。
- 未新增、修改或运行测试，未提交或部署。真实 Stop/Ready 竞争、跨副本通知延迟及取消后崩溃尚未运行验证。
- Docker 长创建、K8s 绑定实例长等待与取消持久回执仍未完成，本批不代表整体任务完成。

### 2026-09-20：Docker 与 K8s 绑定实例的取消补齐

基线 a571a0ea 加累积工作树。K8s 已绑定 builder 的恢复启动在已确认写入后允许中断只读就绪等待。Docker 新建 builder 在创建前、创建成功后及健康等待中响应取消；保留计算资源，不进入普通 agent 的健康失败清理分支。Docker 复用绑定实例在启动前和完整写入/回执返回后检查取消。新增 Docker typed cancellation 映射为运行时 CreationCancelled，不通过错误文本分类。

- `cargo fmt --all`：退出 0。
- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features`：最终退出 0，日志 `/tmp/rcoder-cancel-parity-final-check.log`。
- 未新增、修改或执行测试，未部署、提交、推送。
- Docker create/start 已发出的请求仍完整等待；不会通过丢弃请求 future 宣称取消成功。普通 agent 不响应此取消信号。
- 取消回执落库前进程崩溃的恢复、Docker 创建内部写入阶段响应丢失、历史资源恢复等仍需继续开发；此处不代表整体完成。

### 2026-09-20：取消回执与后台收束

Docker 使用原子发布的 builder-cancel 文件，K8s 使用独立不可变 ConfigMap，在执行者确认停止发出写入后、释放原租约前持久化取消确认。回执绑定完整 execution context 和原租约；不要求资源不存在，不把取消视为创建成功。后台恢复读取回执，核验并释放原租约后，使用完整 operation 快照 CAS 收束 Failed，保存 creation_cancelled 检查点。重复或过期扫描不能收束新执行者。

- 覆盖回执已保存、数据库尚未提交时的退出窗口。原租约释放失败不会返回已确认取消。
- 仍未覆盖执行者返回取消到回执持久化之间的退出窗口；更早未确认的 Docker/K8s 写入仍保持恢复保护。
- 首次全 features 编译退出 101：新增调用触发 unused-qualifications lint；已移除冗余限定。最终 `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-cancellation-receipt-final-check.log`。
- 未新增或运行测试，未操作集群、提交或发布。真实崩溃和多副本时序未验证。

- 默认 features `CARGO_BUILD_JOBS=2 cargo check -p rcoder` 退出 0，日志 `/tmp/rcoder-cancellation-receipt-default-check.log`；格式化及 diff 空白检查通过。

### 2026-09-20：Service 修复归入受理操作

UserApp 查询不再顺带创建 Service。builder 查询实时读取 Service，缺失时返回未具备完整路由的信息；注册交叉验证将其交回 admitted ensure。绑定旧实例的启动完成后，在原租约内执行最终 Service 修复并原子携带创建完成回执，再写独立归档。无需删除原 Pod/PVC。普通 agent 保持既有 self-heal。

- `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 最终退出 0，日志 `/tmp/rcoder-service-read-fence-final-check.log`。格式化和 diff 空白检查通过。
- 未新增或执行测试，未部署、提交或推送；Service 丢失后真实聊天恢复与 Stop 竞争未运行验证。
- 补齐绑定实例最终 Service 写入至归档间的恢复证据；启动写入至 Service 回执之前的未知结果仍须继续处理。

- 默认 features `CARGO_BUILD_JOBS=2 cargo check -p rcoder` 退出 0，日志 `/tmp/rcoder-service-read-fence-default-check.log`。

### 2026-09-20：创建终态前不发布注册

移除 builder 创建执行者在成功终态 CAS 之前的 register_builder。就绪确认复用只读身份/路由校验，不刷新项目注册；成功等待方完成实时身份校验、检查当前 compute 控制状态后再登记。已有后台恢复不登记路径保持一致。创建返回后的平台健康等待可响应同生命周期且明确包含该操作的 compute 接管，不等待健康超时。普通控制的就绪入口保留原签名。

- 未新增、修改或执行测试。未提交、部署、推送。
- 编译中发现控制与 compute 控制也共用就绪 helper；已保留原入口，并以内部可选创建身份开启取消观察，避免混用两种 operation record。
- 登记是缓存，不是运行权威；不能以此宣称所有查询/停止竞态已经验证。其他原计划缺口继续保留。

- 最终全 features 与默认 features `cargo check -p rcoder` 均退出 0，日志 `/tmp/rcoder-builder-registration-final-check.log`、`/tmp/rcoder-builder-registration-default-check.log`；格式化及 diff 空白检查通过。

### 2026-09-20：Docker 复合创建错误保留已发生副作用

核对缺失控制器 Restart 时发现：Docker create 返回物理 ID 后，start 的 4xx 原本直接作为 BollardError 传播，被 builder_completion 误判为整个请求无副作用拒绝。新增 ContainerCreationIncomplete，携带物理容器 ID、阶段和底层错误；创建后的 start 错误、成功启动后 get_agent_info 错误进入此类型。顶层仅对直接的请求拒绝分类，复合创建失败保留恢复状态，不能据最后一次 HTTP 状态自动清理原槽位。

- 全 features `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出 0，日志 `/tmp/rcoder-docker-partial-create-check.log`。
- 未新增、修改或执行测试；未部署、提交或推送。现有直接断言底层错误变体的测试需后续测试阶段核对，未通过调整预期制造验证结果。
- 此改动修复错误分类，不宣称已实现缺失控制器重建。该路径仍需原模板及 PVC UID 的持久化恢复依据，不能用当前配置盲目替代。

- 默认 features `CARGO_BUILD_JOBS=2 cargo check -p rcoder` 退出 0，日志 `/tmp/rcoder-docker-partial-create-default-check.log`；格式化及 diff 空白检查通过。

### 2026-09-20：Docker 部分创建回执进入恢复链

ContainerCreationIncomplete 的阶段改为枚举，禁止通过阶段文本分类。新建 builder 返回错误后，仅在 start 明确拒绝（4xx 排除 408/499）或成功写入后的只读观察失败时，按创建响应物理 ID 检查原生命周期资源，并再次核验目标不变，保存带原租约的 BuilderCreationReceipt。原请求仍报告失败/恢复状态；后台释放原非活跃租约后记录 builder_created_observed。该状态只证明可收束旧写入，不证明业务健康；Stopped 容器不会自动晋升创建成功。

- start 超时、断连或 5xx 无回执，不用 inspect 的瞬时状态推断远端写结束。回执保存/身份验证失败保留原错误与失败阶段。
- 覆盖收到明确部分结果后的恢复，不覆盖 Docker 写响应丢失或回执持久化前退出。
- 未新增、修改或执行测试；未提交或部署。

- 默认/全 features `CARGO_BUILD_JOBS=2 cargo check -p rcoder` 均退出 0，日志 `/tmp/rcoder-partial-creation-recovery-default-check.log`、`/tmp/rcoder-partial-creation-recovery-check.log`；格式化及 diff 空白检查通过。

### 2026-09-20：app-cli 缺凭据时保留控制循环

发现历史部署凭据被脱敏后，restored_runtime_args 在初始化与 server_loop 两处校验，后者直接等待进程取消，导致控制通道不消费。拆分自动启动凭据检查与确认目录解析：初始化仍抑制业务启动并保持恢复保护；控制循环可进入接收状态，Stop 仍由原内核核验。目录/来源/代次不可信的原保护不放宽。

owner 恢复保护接入 kernel 受理，先按原 operation 重放，再拒绝新业务操作，拒绝发生在持久化前；Stop 不受该新增 owner 条件影响，但内核未知操作保护仍保留。派发时再次检查 owner 保护，避免受理后发生恢复异常仍启动业务。OpenAPI 同步说明。

- 独立 app-cli 全 features `cargo check` 退出 0，日志 `/tmp/app-cli-recovery-admission-final-check.log`。
- 未新增、修改或执行测试；未发布 npm、构建镜像或操作集群。
- 本批不代表已完成原操作补交凭据接口；未知内核操作、损坏执行目录等恢复仍按原证据约束处理。

- 独立 app-cli 默认 features `cargo check` 退出 0，日志 `/tmp/app-cli-recovery-admission-default-check.log`；独立格式化及 diff 空白检查通过。

### 2026-09-20：K8s dev Restart 保存并恢复原控制器模板

Restart 在停止前捕获原 StatefulSet（UID/resourceVersion/Pod 绑定不变）与 PVC UID，将完整配置保存到不可变私有 Secret。公开 checkpoint 只保存 Secret 身份、原物理目标和卷身份，不暴露环境变量/凭据。

正常执行与原 stopped 操作恢复均支持：原控制器在停止后丢失 → 核验模板身份及原实例彻底消失 → 验证原 PVC → 创建原配置的零副本 StatefulSet（新 UID，PVC Retain）→ 再次核验卷 → 通过当前执行者/操作 CAS 后启动。替代控制器须带同一 archive UID；同名外来控制器不覆盖。新控制器补齐当前操作/生命周期原生注解，历史旧 UID 的绑定不移用到新资源。

- 这一分支覆盖已有 Restart 模板且停止已确认后的控制器丢失；请求前控制器已经丢失、尚无历史模板，以及 Docker/prod 对等重建仍未完成。
- 已有正常控制器不重建，STS 选型不改变。未删除任何 PVC。
- 首次编译发现共享无实例核验 helper 可见性不足，限定为 runtime 模块内可见后继续编译。
- 未新增、修改或执行测试；未操作集群、提交或发布。真实 RBD、Secret 权限、多副本故障注入未验证。

- 最终根 rcoder 全 features / 默认 features 与独立 app-cli 全 features 编译均退出 0：`/tmp/rcoder-builder-template-restore-complete-check.log`、`/tmp/rcoder-builder-template-restore-default-check.log`、`/tmp/app-cli-builder-template-shared-check.log`。编译修复另包含两个 unused-qualifications；未修改测试。格式化与 diff 空白检查通过。

### 2026-09-20：traffic wake 操作占用快失败

- 生产逻辑：traffic wake 改用 try_acquire_process_release_lock；运行时 prod 租约冲突查询 durable blocker，并保留底层租约定位消息。未删除租约或绕过原执行者。
- 原先获取锁的 200ms 轮询不再消耗该 wake 的整段就绪预算；显式 start/recycle 的等待语义未改。
- `CARGO_BUILD_JOBS=2 cargo check -p app_manager --all-features`：退出码 0，日志 `/tmp/rcoder-wake-conflict-check.log`。
- 按用户要求未新增、未执行测试；未部署。
- 未闭环：WakeOutcome::Failed(String) 仍把结构化冲突降为字符串，ensure HTTP 当前仍映射 ERR_BACKEND_ERROR。后续需增加结构化 outcome 并更新全部消费者；此批不宣称接口透传完成。应用级失败读取、写入回执与只读等待分离、原操作恢复也仍待继续。

### 2026-09-20：wake blocker 贯穿消费链

- 新增 WakeOutcome::Blocked，activity leader 保留 ConflictBlocked 的结构化内容，同进程 follower 共享同一结果。
- prod ensure 返回 ERR_CONFLICT 信封、blocker、非空 operation_id；文件/数据库/生命周期调用方保留冲突类型，HTTP 转发保留结构化响应；Pingora 不放行无就绪上游，仍返回 503。
- 同步 ensure OpenAPI 描述。未伪造 legacy lease 的持久操作 ID。
- 初次编译指出两个未覆盖的生产消费者，已补齐；最终 `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features` 退出码 0（/tmp/rcoder-wake-blocker-check.log）。
- 未新增或运行测试；默认 features、独立 app-cli、部署验证尚未执行。此批只完成冲突传播，不能代表应用级失败诊断和恢复已完成。

### 2026-09-20：无 migrate 服务的 PG 前置声明

- app-cli 的 workspace 判定与 probe 目标筛选共用 service_needs_pg，支持 release.lock 服务 env 中 APP_CLI_REQUIRE_PG="1"。不要求服务必须有 migrate；全局强制开关保持原行为。
- Go 模板已确认启动时执行 gorm.Open，且 manifest 无 migrate；对应模板清单补充该声明。模板仓本轮修改前工作树干净。
- `CARGO_BUILD_JOBS=2 cargo check --manifest-path crates/app-cli/Cargo.toml` 退出码 0（/tmp/app-cli-pg-dependency-check.log）。未新增或运行测试。
- 尚未发布模板或 app-cli；存量制品需重新构建纳入声明，或由既有全局 APP_CLI_REQUIRE_PG=1 配置启用。不能据此宣称现场 app141 已恢复。其他数据库模板声明与模板文档仍待统一核对。

### 2026-09-20：其余后端模板 PG 依赖声明

- 核对 Java JDBC 配置、Python SQLAlchemy 初始化、Rust PgPool 连接，确认三个后端均依赖 PostgreSQL，且清单均未声明 migrate。
- 模板仓 backend-java/backend-python/backend-rust 清单补 APP_CLI_REQUIRE_PG="1"，与 Go 一致；未给纯前端模板添加数据库依赖。
- 模板 docs/03-manifest-reference.md 补充声明、SELECT 1 探测、psql 前置、不可替代应用重连、存量制品需重新构建等说明。
- 模板仓 git diff --check 退出码 0；本批仅清单和文档，无新增/执行测试、无发布。Next.js 模板的数据库依赖需按实际可选功能再核对，不因全栈名称盲目开启。

### 2026-09-20：wake 读取容器内部失败状态

- wait_for_captured_wake 在基础状态未就绪时，通过 capture_app_configuration_target + exec_app_configuration_target 读取容器 loopback /ready，不依赖 Ready Service。
- 捕获部署代次及物理 UID，读取后复核相同物理目标，并再次确认原 workload UID；仅明确 phase=failed 时提前报告应用启动失败、代次和 wake operation_id。
- 单次只读观察最多 4 秒，仍受外层原 wake deadline 限制。管理通道尚不可用、响应不完整或协议无 phase 时继续原等待，不推断成功或失败；phase=failed 不是租约释放证据，原恢复保护保留。
- cargo check -p app_manager --all-features 退出码 0，日志 /tmp/rcoder-wake-startup-check.log。随后将序列化比较简化为已有 PartialEq 比较；fmt check 通过。未新增/运行测试，未部署。
- 尚待：结构化服务失败明细、写入确认和只读等待的持久阶段分离、原操作恢复；本批不能释放现场残留租约。

### 2026-09-20：wake 写入确认的持久边界

- 在 start_app_target 完整返回后、任何 readiness 观察之前，持久化 traffic_wake_observing，保存原 target 与 start_write_acknowledged=true。
- 核对 K8s 实现包含存储 claim 和条件 Deployment patch，只有完整返回才记确认；Docker 实现为捕获 container ID 的 start，304 同属明确返回。
- checkpoint 写入失败/取消仍保留原操作保护；没有将内存确认标记作为解锁依据。旧 traffic_wake_target 无此证据，不能推断已确认。
- fmt check 及 CARGO_BUILD_JOBS=2 cargo check -p app_manager --all-features 退出码 0，日志 /tmp/rcoder-wake-observing-check.log。
- 未新增/运行测试；尚未实现根据该 checkpoint 自动收束及释放租约，后续需接入原操作 CAS 与恢复入口。

### 2026-09-20：当前 wake 的只读观察失败收尾

- OwnedOperation 校验 Start + traffic_wake_observing + acknowledged=true + checkpoint target 的完整执行上下文等于当前 executor，才能进入 fail_confirmed_wake_observation。
- 启动写链已返回、持久阶段已确认后的只读错误/超时，先按 lifecycle/revision/executor CAS 写 Failed 并保存原证据，再 mark_completed 和释放精确租约。该 Failed 仅表示本次等待未成功，不声称业务已停止或永不就绪。
- 原阶段持久化失败、远端写结果未知仍沿用 RecoveryRequired；终态 CAS 失败不释放租约。Stop 接管造成的执行身份变化由存储拒绝提交。
- app_manager 全 features check 退出码 0：/tmp/rcoder-wake-settle-check.log；未新增/运行测试、未部署。
- 本批仅收尾当前仍持有原操作的请求。历史 RecoveryRequired 的显式恢复及终态提交后崩溃遗留租约仍需后续对账，不能将该路径直接套用于旧无确认 checkpoint 操作。

### 2026-09-20：已确认 wake 的 RecoveryRequired 后台收束

- 新增存储 finalize_observed_wake，在共用短事务实现中核对完整记录相等、Start、RecoveryRequired、traffic_wake_observing、写入确认标记以及 target 的 app/lifecycle/operation/executor/fingerprint。
- 仅作内部终态转换，不重新执行启动；Failed 保留原 checkpoint 和错误，事务同时更新根/槽位/操作并清理原执行输入。
- 后台恢复扫描接入该分支，终态回执清理复用已有 discover_terminal_leases：释放原精确租约后才忘记绑定。无需依赖租约年龄或猜测运行状态。
- 无确认 checkpoint 的历史操作、Running 执行者不进入该路径；现场 app141 旧记录不因该改动自动解锁。
- rcoder 全 features check 退出 0（/tmp/rcoder-wake-recovery-check.log）；首轮 unused qualification 已修复。未新增或运行测试，未部署。

### 2026-09-20：已确认 wake 接入原 retry 入口

- 现有 /api/v1/userapp/{app_id}/operations/{operation_id}/retry 在 lifecycle/revision 校验后，支持 traffic_wake_observing RecoveryRequired 原记录收束，以及 traffic_wake_observation_failed Failed 原回执清理。
- 先完整记录 CAS 再释放，与后台扫描共用存储语义；释放前核对绑定 app/lifecycle/operation/executor/fingerprint；原精确回执释放成功后才 forget。
- 返回原 Failed 记录，不把恢复清理报告为业务启动成功。若释放失败，查询最新 revision 后可重试终态清理。并发扫描通过同一精确租约身份避免误删新锁。
- 同步接口说明。app_manager 全 features check 退出码 0（/tmp/rcoder-wake-retry-check.log），git diff --check 通过；未新增/运行测试，未部署。
- 无 acknowledged checkpoint 的旧 wake 不在此分支，仍需其他物理写回执及执行收束证据；未通过降级校验自动接管现场旧操作。

### 2026-09-20：按用户修订，PG 等待归运行环境

- 撤回四个后端模板的 APP_CLI_REQUIRE_PG 声明；清单文档改为说明平台注入，覆盖此前服务级声明方案。
- app-cli 仅以进程环境 APP_CLI_REQUIRE_PG=1 开启前置探测，不再因 migrate 或模板 env 自动启用。独立客户端数据库由客户端准备；显式启用时仍尊重 DATABASE_URL/PGHOST/PGPORT 等目标配置。
- RCoder prod create/update 公共参数装配默认注入 1；dev 在 Docker AgentContainerStarter 与 K8s agent env 的真实服务配置合并处默认注入 1，可由运行环境配置覆盖。未依赖 dev 忽略的通用 params.env 字段。
- 独立 app-cli check 退出 0，日志 /tmp/app-cli-pg-policy-check.log；根 rcoder 全 features 编译日志 /tmp/rcoder-pg-policy-check.log。未新增/执行测试、未发布；存量容器需后续部署更新环境。

### 2026-09-20：wake 带出部署错误详情

- 身份绑定的容器 exec 读取 /v1/deploy/status，兼容信封/裸 JSON，以 AppCliDeployPhase 判定失败。
- 部署 operation 存在时要求 deployment_generation_id 与捕获目标相同，再提取 error（最多 2048 字符）；不要求旧部署 operation_id 等于新 wake ID。代次不符或结构无效继续观察，不挪用旧错误。
- 仍前后核验物理实例及 workload UID；无 operation 详情时只报告通用启动失败。错误文本只用于诊断，不用于租约释放或业务分支。
- app_manager 全 features check 退出码 0：/tmp/rcoder-wake-detail-check.log；diff check 通过。未新增/运行测试、未部署。

### 2026-09-20：迁移输入预检在意图回执之前

- 核对两种编排引擎共用 run_migration_with_receipt_cancel；已成功的同制品迁移由 MigrationJournal 跳过，未确认迁移仍阻止自动重跑。
- 修复本地确定性输入错误先污染迁移回执的问题：命令为空、空 executable、argv NUL、工作目录不存在/非目录，在 MigrationJournal::begin 前失败；底层 transient 执行也复用预检。
- 预检不证明 spawn 成功；意图已记录后的进程创建/执行未知仍保留原保护，没有删除未知迁移记录或伪造完成。
- 独立 app-cli 默认 check 退出码 0（/tmp/app-cli-migration-preflight-check.log）；diff check 通过。未新增/执行测试，未发布。

### 2026-09-20：重启模板归档边界

- K8s builder Restart 的新归档名称改为 namespace/context/物理 source 的 SHA-256，分成两个 32 字符 DNS 段，避免长 operation ID 编码后超过名称限制。旧 checkpoint 继续按原 archive name/UID 读取。
- 恢复前核对归档 workload 的 name/UID/resourceVersion 与原 source 完全一致，随后仍按零副本创建及原 PVC UID 验证流程执行。
- docker_manager 补 workspace sha2 依赖；适配 0.11 输出数组的 hex 编码。未新增/运行测试；组件编译日志 /tmp/rcoder-restart-archive-check.log。

### 2026-09-20：成功重启的私有归档清理

- 新增运行时 cleanup_builder_restart_archive；K8s 按 namespace/name/UID/resourceVersion 删除原 Secret，404 幂等，同名替换拒绝删除。
- compute 后台扫描只对 Succeeded 且 checkpoint 含归档的记录处理，核对 app/lifecycle/operation 后先释放原租约，再删除归档。此前已忘记租约的成功记录同样进入清理，覆盖完成后退出窗口。
- Failed/RecoveryRequired 继续保留模板；不删除 PVC 或控制器。当前清理幂等扫描会重复读取已删除归档（404），尚未增加独立 GC 完成标记，后续可优化扫描成本。
- 未新增/运行测试。全 features 编译日志 /tmp/rcoder-archive-cleanup-check.log；无部署。

### 2026-09-20：归档清理扫描补全与损坏记录隔离

- 更正上一条记录：原 scan SQL 没有包含已忘记租约的成功记录；最新代码增加未清理归档筛选，并通过完整快照/revision CAS 写清理标记。真实 PG/Turso SQL 尚未运行验证。
- 单条归档 JSON 损坏或操作身份不匹配时，记录错误并保留该记录的租约和归档，继续处理同页其他操作；不再中断整页终态扫描。
- cargo fmt --all -- --check、git diff --check、CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features 均退出 0；编译日志 /tmp/rcoder-recovery-scan-isolation-check.log。未新增或运行测试，未部署。
- 原操作凭据补交通道及交接文档 R1–R6 仍需继续开发，本批不代表完整恢复功能完成。

### 2026-09-20：归档删除确认

- K8s DELETE 返回后实时 GET 核验原 Secret UID 已消失；仍 Terminating 时返回明确待清理错误，不写 GC 完成标记，后续扫描继续处理。
- 同名新 UID 表示原对象已消失，直接完成原归档清理，不删除新对象；覆盖旧删除成功但响应丢失后重试。
- docker_manager kubernetes 编译退出 0（/tmp/rcoder-archive-delete-confirmation-check.log）；编译后进一步将首次读取到新 UID 的分支改为幂等完成，该最后分支改动只做 fmt/diff 检查，未重新编译。未新增/运行测试、未部署。
- 原操作凭据补交通道仍需继续实现；本批没有开放受保护的业务重放。

### 2026-09-20：停止意图与 Source 凭据恢复标记

- initialize_startup 先解析已确认目录并读取 Stopped；停止状态直接 Idle，不因旧密码已脱敏或迁移预检而产生新的启动 hold。已有 kernel 恢复保护不清除；自动恢复业务仍执行凭据和迁移检查，显式编排仍遵守迁移 journal。
- Source 编排的部署请求保留 run_pg 输入，journal 序列化只保留用户名和空密码；此前错误记录 None 会让重启误认为环境凭据足够。真实密码仍通过原 pending 配置传给编排，不落盘。
- 独立 app-cli cargo check 退出 0（/tmp/app-cli-source-credentials-check.log），fmt/diff 检查通过。未新增或运行测试。原操作补交凭据入口仍未闭环。

### 2026-09-20：owner 恢复证据查询

- 增加 GET /v1/runtime/recovery，复用 owner token，返回共享 RuntimeRecoveryView：当前实例/代次/revision、两层保护、原部署 operation、journal boundary、代次是否匹配及已脱敏凭据需求。无密码、无解除保护或派发副作用。
- boundary 为诊断字段，调用方必须容忍未知值；凭据需求不证明仅补凭据即可恢复。查询不是原子跨状态提交凭证，后续写必须重新核验。独立迁移核验明细和补交/续行写入口仍未接通。
- 独立 app-cli 编译退出 0（/tmp/app-cli-recovery-view-check.log）；编译后补启动初始化期间返回 503 的门控，最后门控待下一轮编译覆盖。未新增/运行测试。

### 2026-09-20：恢复查询的迁移状态

- 迁移回执检查复用结构化 bool 结果：明确未完成与读取/解析错误分离；执行 begin 仍在原租约下重新检查，未知不重跑。
- recovery 查询在代次一致且有已记录执行目录时返回 migrations=confirmed/unconfirmed/unreadable，否则 not_inspected；不以未检查当作无迁移。无回执目录表示没有已记录未知迁移，不代表数据库 schema 已验收。
- app-cli 编译退出 0（/tmp/app-cli-migration-recovery-view-check.log），随后将查询 journal 快照复制后释放 guard，避免持锁扫描文件；该锁范围微调仅 fmt/diff 检查，待后续编译覆盖。未新增/运行测试。补交凭据及续行仍未完成。

### 2026-09-20：已确认 Source 的显式凭据启动

- owner hold 改为独立位：缺凭据与其他未知结果。未知结果永不由补凭据清除；辅助写失败同样设置未知位。
- 仅缺凭据且已确认停止、代次匹配、Active/RestoredActive/StartupFailed、Source 目标及制品一致、迁移回执确认时，现有 runtime operations 接受携带 PG 的显式 Start/Restart。kernel 原实例/revision/恢复/冲突检查仍执行，派发前复核，再 CAS 清除仅凭据位。
- 缺凭据不再虚报 shutdown_unconfirmed；其他启动失败仍保留其原保护。新请求不复活旧未知操作，不重放迁移，不写密码文件。
- 独立 app-cli 编译退出 0（/tmp/app-cli-source-credential-resume-check.log）；fmt/diff 通过；未新增或执行测试。
- 此分支针对旧执行已经终结的显式新启动。RecoveryRequired 原操作补交与续行、Artifact 目标及平台调用仍未全部接通，不能视为 R4 完成。

### 2026-09-20：显式 Artifact 的凭据保护恢复

- 受理及派发共用 can_supply_run_credentials；Source 请求仍要求旧确认目录为 Source，Artifact Deploy 可从已记录的 Source/ProjectRun 确认版本进入正常部署流程。
- 校验实际已确认目录的 release.lock 及迁移回执，沿用实例/revision/kernel 保护；派发校验错误记录上下文且保留保护，不静默清除。
- 未标记 execution_target 的旧 journal 仍需身份恢复，不能猜目录。本批是用户显式新部署，未冒充原 RecoveryRequired 续行。
- app-cli cargo check 退出 0（/tmp/app-cli-artifact-credential-resume-check.log）；diff 检查通过。未新增/运行测试、未部署。

### 2026-09-20：旧本地制品执行目录恢复

- 当前 owner 的 journal 租约下，代次一致、已有 active 且边界允许时，核对 Source/.run 的 release ID。仅一个匹配才为缺标记的 local artifact 原 active 请求补 execution_target 并持久化；不同失败请求不被改写。
- 多匹配、无匹配、读取错误保持恢复保护；不靠名称选择，不切目录、不启动进程、不重新迁移。URL 制品未套此规则，避免把部署目录误解释成 dev profile。
- app-cli check 退出 0（/tmp/app-cli-legacy-directory-recovery-check.log），diff 检查通过。未新增或运行测试。原操作续行及其他剩余项仍未完成。

### 2026-09-20：app-cli 取消意图持久化

- request_cancel 在 admission 屏障内先写 cancellations 原子回执（operation_id + request_digest），成功后才返回。持久化失败仍在本进程阻止成功提交并保留保护。
- owner recover 恢复未终态操作的取消墓碑；损坏/不匹配回执记录错误并保留保护，不把取消信息读取失败当未取消，也不让单条损坏直接退出管理恢复流程。
- 不提前写 Cancelled，清理仍由执行边界确认；终态取消回执保留作历史证据。旧版本没有持久取消证据，不能据 Active journal 一概推断成功，原终态恢复尚未开放。
- app-cli check 退出 0（/tmp/app-cli-durable-cancellation-check.log），diff 通过。未新增或运行测试。

### 2026-09-20：StartupFailed 原操作终态恢复

- 仅在启动已确认旧 owner 停止后，读取原 StartupFailed journal，核对代次、当前制品、迁移回执、已持久化部署成功阶段、原 runtime operation ID/workspace/摘要；原记录须为 RecoveryRequired 且没有活跃执行槽。
- 保存原记录为 Failed；有有效持久取消回执时为 Cancelled。不派发、不启动、不迁移，不推断 Succeeded。剩余非终态/损坏记录继续保持全局保护。
- 新 cancellation 目录纳入旧状态根迁移权威文件清单，避免迁移漏掉取消意图。
- app-cli check 退出 0（/tmp/app-cli-startup-failure-reconcile-check.log），diff 通过。未新增/运行测试；Active 成功窗口、未知切换和补交原操作输入仍未全部完成。

### 2026-09-20：Active 的原操作提交中断

- 同一启动静止确认入口增加 Active+Running journal：仍验证原代次、制品、已持久化部署阶段和迁移，原操作必须是 RecoveryRequired 且没有活跃执行槽。
- 无已提交终态时保存 Failed/ERR_OPERATION_INTERRUPTED，明确是终态提交前 owner 已停止，不将旧 Running 观察补报 Succeeded；有取消回执时保存 Cancelled。
- 不重新执行原操作；之后自动恢复仍受 desired、凭据和 journal 检查约束。其他未完成记录保留保护。
- app-cli check 退出 0（/tmp/app-cli-quiesced-operation-check.log），diff 通过。未新增/运行测试，未部署。原未知切换和其余交接范围继续保留。

### 2026-09-20：原 Stop 的终态恢复

- 在 owner 启动静止确认之后，核对 desired=Stopped、原 Stop revision、原请求 expected_revision+1、workspace/operation/digest，并要求唯一候选和无执行槽，才将对应 RecoveryRequired 收束为 Succeeded。
- 不修改 desired/revision，不发新 Stop，不创建进程。其他未知业务操作或损坏记录仍保护；不因一个 Stop 已成功就清掉全部恢复状态。
- app-cli check 退出 0（/tmp/app-cli-quiesced-stop-check.log），diff 通过。未新增/运行测试、未部署。

### 2026-09-20：Preparing 原操作恢复

- 已确认旧 owner 静止后，Preparing + Pending/Failed 部署段 + Deploying/Failed 相位可按原身份收束中断操作；源码核对目录激活前必先持久化 Switching，因此不将 Switching/Activated 纳入此分支。
- 有旧 active 时仍验证其制品和迁移；首次部署无 active 允许原准备操作结束，不编造旧版本。后续启动仍走原 journal/迁移/desired 检查。
- 保存 Failed/ERR_OPERATION_INTERRUPTED 或有取消回执时 Cancelled；不派发、不切目录、不伪造成功。
- app-cli check 退出 0（/tmp/app-cli-preparation-reconcile-check.log），diff 通过；未新增/运行测试。未知切换与其余原计划仍未完成。

### 2026-09-20：Activated 启动中断恢复

- 启动确认旧 owner 静止后，对 Activated 核验当前制品与迁移回执；generation 一致、部署阶段已确认持久化成功、无额外 pending recovery 才允许原 journal 完整快照比较后转 StartupFailed。
- 有 runtime operation 时只处理原 RecoveryRequired，不改写已提交终态；legacy 无 runtime operation 的 journal 也能记录启动中断。持久化失败设置保护并保留错误。
- 后续原操作收束复用同一失败恢复路径。新 journal 保留制品与请求，不回滚目录、不自动启动、不重新运行迁移；用户显式操作可沿现有确认版本继续。
- app-cli check 退出 0（/tmp/app-cli-activated-reconcile-check.log），diff 通过。未新增/运行测试、未部署。Switching 仍需目录交换证据核验，尚未闭环。

### 2026-09-20：Switching 目标身份前置

- 已验证 staging 提供目标 release ID；Source 则在切换意图前读取其 release lock。两者先更新本操作的目标 artifact_release_id，再持久化 Switching，之后才激活。
- 不提前替换当前 release 或 journal.active，因此旧版本身份仍可用于核验和准备失败恢复。旧无目标证据 Switching 记录不推断为新协议。
- app-cli check 退出 0（/tmp/app-cli-switch-intent-check.log），diff 通过。未新增/运行测试。目录切换结果核验仍待后续接入。

### 2026-09-20：Switching 新目录已就位的恢复

- 启动静止确认后，Switching 必须已有目标 artifact ID，代次匹配，目标实际 release lock 一致且迁移回执确认。按 journal 完整快照比较补 Activated，再复用启动中断记录和原操作收尾。
- 只改原持久证据，不搬目录、不自动执行迁移；目标缺失/身份不符/旧无目标记录继续保护。
- journal.resume 拒绝时显式设置 owner hold，避免 legacy 无 kernel 操作的未知切换被普通 fail_operation 覆盖原证据。
- app-cli check 退出 0（/tmp/app-cli-switch-observation-check.log），diff 通过；未新增/运行测试。旧目录尚在、目标目录缺失的恢复仍需继续，其他原计划残留未完成。

### 2026-09-20：Switching 旧制品仍在位

- 新目标不匹配时，仅实际 release 等于原 active、执行目录绑定相同、迁移确认且无其他恢复阶段，才能按 journal 完整快照将本次尝试记为失败并保留旧 active。
- 不搬动目录、不把旧制品记成新制品；原操作保留实际切换中断错误，不统一误写成从未进入切换。
- app-cli check 退出 0（/tmp/app-cli-preserved-active-check.log），diff 通过。未新增/运行测试；workspace 缺失而仅 .previous 存在的窗口仍需恢复协议，不能用本批覆盖该场景。

### 2026-09-20：缺失 workspace 的旧目录恢复

- Switching 中目标目录缺失时，要求原 active 和执行目录绑定一致、无其他 recovery，独占 preparation lease 后校验 .previous 是真实目录且 release ID 匹配，迁移回执确认后恢复。
- Linux/macOS 使用 rustix 安全 API 的 NOREPLACE，Windows 使用目录 rename（已有目标会失败）；不删除目标、不覆盖后来出现的目录。Unix 同步父目录，恢复后再次核对 release ID，再走原 journal 失败收束。若 rename 成功但后续持久化失败，下次可由旧目录在位分支继续核验。
- app-cli 增加 Linux/macOS rustix fs 依赖，macOS cargo check 退出 0（/tmp/app-cli-previous-generation-check.log）；diff 通过。未新增/运行测试，Linux/Windows 此分支尚未编译或实机验证。

### 2026-09-20：URL 制品凭据恢复目录绑定

- owner 保存不可变启动 workspace；旧 HTTP(S) URL active 请求无 execution_target 时，显式 runtime Artifact Deploy 的凭据保护核验使用该绑定目录，并继续核对 active release 和迁移。
- 本地制品缺 provenance 不走此分支；Source 操作仍要求明确 Source 目标。没有把 URL 部署猜成源码开发模式。
- app-cli check 退出 0（/tmp/app-cli-url-credential-recovery-check.log），diff 通过。未新增/运行测试。平台仍需核查/接入 runtime 入口，legacy /v1/deploy 的 hold 行为未改变，不能声称生产调用全链已闭环。

### 2026-09-20：平台 legacy 部署入口复用凭据核验

- `/v1/deploy` 的受理核心复用 can_supply_run_credentials，只有凭据保护且原制品/迁移已确认、kernel 无恢复保护时允许显式携带 PG 的新部署。保留原 operation_id、generation 和原状态轮询接口。
- admission 锁内 compare_exchange 只消费 credentials-only 位；持久化或发送失败自动恢复该位，不覆盖未知状态位。OpenAPI 补充恢复拒绝语义。
- CARGO_BUILD_JOBS=2 cargo check --manifest-path crates/app-cli/Cargo.toml 退出 0，日志 /tmp/app-cli-legacy-credential-admission-check.log；git diff --check 退出 0。未新增/运行测试。
- 这只接通旧入口受理；受理后准备失败的凭据保护连续性、原未知操作补交和 hot_execution 仍需完成，不代表平台全链验收。

### 2026-09-20：制品缓存命中不丢运行凭据

- prepare 返回 None 仅代表制品文件可复用。Idle/Failed 部署链现在仍绑定本次 workspace、profile 和 PG；Running 热部署链携带 PG 时返回业务重编排动作，经旧服务停止后执行，不直接报告配置已生效。
- 凭据补交核验允许 Preparing+Failed 且保留确认 active 的记录，仍核对制品、目录和迁移；不放行执行中的 Preparing。
- app-cli cargo check 退出 0（CARGO_BUILD_JOBS=2，/tmp/app-cli-cached-release-credentials-check.log），git diff --check 退出 0。未新增/运行测试。
- 凭据恢复受理后失败的保护位完整生命周期尚需继续处理；本批不宣称 R4 全部完成。

### 2026-09-20：凭据恢复临时放行绑定操作身份

- credentials-only hold 的消费绑定 operation_id，由旧部署受理及 runtime dispatch 共用；不是无身份布尔标记。
- 受理失败由 guard 恢复原凭据保护；执行失败/取消在内核释放槽位前按原 ID 恢复，legacy fail_operation 同样处理。未知保护位不清除。
- 确认 Running 的 journal 持久化完成后才消除该身份；Stop 的执行循环确认业务停止后收回 legacy 未完成放行，不让其缺少 kernel callback 导致遗留。
- Preparing+Failed 重试仍验证保留 active 的制品、目录和迁移；此分支不处理未知迁移和原操作输入补交。
- CARGO_BUILD_JOBS=2 cargo check --manifest-path crates/app-cli/Cargo.toml 退出 0，/tmp/app-cli-credential-operation-fence-check.log；git diff --check 退出 0。未新增/运行测试，未部署。

### 2026-09-20：hot_execution 物理身份见证

- 平台热部署在提交前捕获原 Pod UID/container ID 与 deployment generation，和 context、release_id、receipt_protocol=1 一起持久化于 hot_execution；不存令牌、密码或带签名 URL。
- 若之前已执行显式 PG 对齐，要求数据库目标与热部署目标一致；执行前后重新核对目标，不把替换实例状态当原操作完成。
- 此处观察并非跨远端写原子事务，仍不能凭资源暂时不存在解锁；HTTP 提交的不确定结果继续保持恢复。
- CARGO_BUILD_JOBS=2 cargo check -p app_manager --all-features 退出 0（/tmp/app-manager-hot-physical-target-check.log）。未新增/运行测试。后续仍需接入原 hot_execution 的状态核验、配置收敛和存储 CAS 终结；本批只补可靠恢复输入，不宣称完整恢复已交付。

### 2026-09-20 深夜：R1 Restart 缺失控制器重建（Docker dev + K8s prod），本会话

按《下一轮交接》顺序 F1→R1 开发。只补生产逻辑；按用户指示不新增/不运行测试，仅编译与 fmt 检查。未提交、未推送、未部署。

**F1**：`cargo fmt --manifest-path crates/app-cli/Cargo.toml` 修复 server.rs:2988 单点违规，`--check` 退出 0。

**R1-Docker-dev**（新文件 `crates/docker_manager/src/runtime/docker_builder_restart.rs`）：

- 根因确认：动态 builder 容器 `auto_remove(true)`（agent_container_starter），stop 即被 daemon 删除——Restart 缩容后容器必然消失，此前断在 `Original restart template is unavailable`。
- `archive_builder_restart`（Docker 实现）：归档前双捕获防漂移；从 inspect 归档镜像/env/binds/网络/资源限制（builder env 不含凭据——凭据走 runtime 操作通道，已核实组装链）；bind 见证（HostPath 身份=容器目标+宿主源路径）；确定性内容寻址文件名（sha256），`.app-operation-receipts/<app_id>/builder-restart-<digest>.json`，0600、原子 hard_link 发布、O_NOFOLLOW 读、96KiB 上限。
- `restore_builder_restart`（Docker 实现）：内容哈希==检查点 UID 核验；旧 UID 必须已 404；同名占用仅接受本归档 RESTORE 标记且非运行态（中断重入幂等）；bind 源目录必须存在；镜像 ensure 后**停态重建**（不启动——Starting CAS 先于新业务实例启动的不变量与 K8s 零副本对齐）；身份 label 从当前 context 重推导（不重放旧 label）+ `rcoder.io/restart-archive-uid` 标记；重建后卷见证比对。
- `capture_builder_compute_volumes`（Docker）+ `verify_recovered_volumes`（Docker：HostPath 源目录存在性；防 shared 恢复路径因非空见证踩默认拒绝）。
- `cleanup_builder_restart_archive`（Docker）：仅内容匹配时删除，幂等。
- 顺带修复真实缺陷：`apply_builder_compute_mode` stop 后 inspect 404 的提前返回跳过了 `save_stop` 回执（auto-remove 场景恢复观察者无法确认停止）——现 404 分支先持久化回执再返回。
- AppResourceKind 新增 `HostPath`/`File` 变体（serde 加法兼容；k8s_app_deletion 的 exhaustive match 补拒绝臂）。

**R1-K8s-prod**（新文件 `crates/docker_manager/src/runtime/k8s_app_restart.rs`）：

- trait 新增 `archive_app_restart`/`restore_app_restart`/`cleanup_app_restart_archive`（UserAppDeploymentRuntime，默认 None/Unsupported，fail-closed）；shared_types 新增 `AppRestartTemplate`。
- 归档：Deployment uid+resourceVersion+无删除时间戳核验 → 复用 `prepare_captured_compute_start`（完整身份链+PVC 见证）→ 漂移复查 → 不可变 Secret（label `app-restart-template`，确定性摘要名，900KiB 上限）。
- 恢复：同名 Deployment 存在则必须持本归档 RESTORE 注解；不存在则 `app_compute_absent` + `verify_recovered_volumes(Prod)` 后按归档 spec **零副本**重建（剥 stop/start 回执注解 + RESTORE 标记 + context 元数据）；重建后 PVC 见证比对。
- 接线（compute_control.rs）：prod Restart prepare 后归档入检查点 `app_restart_template`；执行路径与 resume 路径的 UID 变化分支从硬失败改为按归档恢复（含 `Restart target changed after stop` 处）；模板引用贯穿 Starting/Verifying 检查点（中断后续行仍有恢复源）；成功终态后内联清理归档（dev builder 同步受益；崩溃窗口由既有扫描 GC 覆盖，app 模板的扫描集成留给 R2/N1 批）。

**明确延后**：Docker prod 应用容器缺失重建——其 env 内联 secrets（params.secrets 合并），归档重放会把凭据落盘，违反"归档不含明文密码"；需凭据重注入通道（与 R2 凭据恢复重叠），且 Docker prod 容器非 auto_remove、仅在人为删除场景触发，维持原 fail-closed。

**验证**：`CARGO_TARGET_DIR=target-check CARGO_BUILD_JOBS=4 cargo check -p rcoder -p docker_manager --all-features` 退出 0；`-p docker_manager`/`-p rcoder` 默认 features 退出 0；`cargo fmt --all -- --check` 退出 0。未运行任何测试、未部署。R1 剩余：Docker prod（如上延后）与全部实测验收（测试阶段统一执行）。

### 2026-09-20 深夜二批：R2a prod 创建"已提交未记账"崩溃窗口——身份回执采纳

**窗口精确定位**：恢复扫描对在途 `Command::Create` 会重跑 `execute_creation`（lifecycle/recovery.rs:678-690）；其入口 `get_deployment_status(...).is_some()` 即盲拒 `AlreadyExists`——包括"本操作崩溃前已提交复合写、但 `runtime_created` 检查点未落"的情形，操作从此无法自收束。中途崩溃（部分资源）由 K8s ensure 幂等性自然收敛，无需新机制。

**方案**：创建时 Deployment 注解/Docker 容器 label 已带完整操作身份五元组（application/lifecycle/operation/executor-id + request-fingerprint，`resource_metadata()` 单一事实源）——**存活资源即持久回执**，不新增集群对象。

- shared_types 新增 `validate_operation_metadata`（五键严格相等；同生命周期他操作不匹配——重创建是新承诺）。
- trait 新增 `verify_committed_creation(context) -> Option<ContainerBasicInfo>`（UserAppDeploymentRuntime，默认 None=fail-closed 走原 AlreadyExists）。
- K8s：Deployment 按名取，删除中/身份不符→None；匹配→返回与 create_deployment 同形的资源视图（Service FQDN）。
- Docker：容器按 `app_deployment_name` 取，404→None；label 五元组匹配→返回带真实 IP 的视图。
- `execute_creation`：status 存在时先验承诺——本操作身份精确匹配→采纳（跳过 create，直接 register_pingora + `runtime_created` 检查点）；无身份上下文/身份不符/未盖戳→维持原 `AlreadyExists` 拒绝。采纳路径同样 `mark_mutating`（操作确已改资源）。

**修正记录**：Docker 首版误把"运行中"当排除条件（混淆 restart-restore 语义）——创建采纳恰恰要接纳运行中容器，已改回仅以身份为准。

**明确剩余**（下批）：N1 既有回执族 GC（builder 创建回执 ConfigMap/文件在终态确认后的清理与 CAS 标记）、R5 旧 wake 锁收束（消费本批的旧写核验能力）、Docker prod 重启归档（凭据重注入通道）。

**验证**：`CARGO_TARGET_DIR=target-check CARGO_BUILD_JOBS=4 cargo check -p app_manager -p docker_manager -p rcoder -p shared_types -p container-runtime-api --all-features` 退出 0；`cargo fmt --all -- --check` 退出 0。未运行测试（阶段约束），未提交未推送。

### 2026-09-20 深夜三批：R2b/N1 回执族 GC（创建/取消回执 + compute stop/start 回执文件）

**设计**：GC 的迭代对象是回执本身（K8s ConfigMap / Docker 文件）——删除即自清，无需 CAS 标记（对比 restart 归档扫描按记录迭代才需要 `mark_cleaned` 标记）。前置条件遵守"终态确认"：

- trait 新增 `list_builder_creation_receipt_contexts` / `cleanup_builder_creation_receipts` / `cleanup_compute_receipt_files`（UserAppDeploymentRuntime）。
- K8s（k8s_creation_receipt.rs）：label `in (builder-creation-receipt, builder-cancellation-receipt)` 列举，载荷逐个解码+validate 出 context；删除前重读载荷核对归属（同名外来对象绝不删），UID+resourceVersion 前置条件 DELETE，404 容忍。
- Docker（docker_compute_receipt.rs）：回执根目录扫描 `builder-{create,cancel}-*.json`；损坏文件显式报错（不静默跳过、不删除）；删除前内容核对归属；同步补 `cleanup_compute_receipt_files`（builder/prod × stop/start 四文件，内容核对后删）。
- rcoder：恢复扫描器新增 `sweep_builder_creation_receipts`（慢周期 72 tick ≈ 6min：列 context → store 查操作记录 → 终态才清理；记录缺失或非终态保留证据）；`discover_compute_leases` 终态任务的租约释放后顺带清理该操作的 stop/start 回执文件（保持租约门控——无租约终态记录本不应有这些文件，且避免每 5s 空转任务）。
- 途中修正：曾把无租约终态记录也排队清理——会造成扫描器永久空转，已回退为租约门控。

**验证**：`cargo check -p rcoder -p docker_manager --all-features` 与默认 features 均退出 0；`cargo fmt --all -- --check` 退出 0。未运行测试（阶段约束），未提交未推送。剩余：R5（旧 wake 锁）、R3（prod 接管）、R4（converging 终结链）、R6（文档）。

### 2026-09-20 深夜四批：R5 旧版本 wake 锁——运行时回执证据收束

**窗口**：新代码在启动写入全链返回后才持久化 `traffic_wake_observing` + `start_write_acknowledged=true`；旧版本记录（升级前产生）处于同 step 但无该字段——`finalize_observed_wake` 的全记录 CAS 前置（ops.rs:1285）永远拒绝，wake 锁永久占位阻塞新业务。

**方案**（区分"已确认启动但观察未完成"与"未确认写入"）：

- 存储新增 `finalize_legacy_observed_wake(snapshot, evidence)`（trait + Turso/PG 共用 common 实现）：前置条件与常规版相同的身份链/step/kind/state，但要求 `start_write_acknowledged` **不存在**（legacy 判别）；证据（调用方已核验的 runtime start 回执上下文）写入 checkpoint `legacy_start_write_verified` 留审计痕迹后，走与常规版相同的诚实 Failed 终结（"写入已确认+观察未完成"不伪造成功），随后既有路径释放原租约解除阻塞。
- recovery 接线（recovery.rs retry 的 wake 分支）：有 flag → 原路径；无 flag → 从 checkpoint 取原 target → `reconcile_app_compute_start`（K8s=Deployment UID+`rcoder.io/compute-start-receipt` 注解==本操作 context+replicas=1；Docker=prod-start 回执文件内容匹配+容器运行）→ 确认→ legacy 终结；**不确认→保持保护**，错误信息明示恢复路径（人工核验或显式 Stop——Stop 优先级可接管）。不按租约超时/业务 Failed 自动释放，不手工标成功。

**验证**：`cargo check -p rcoder-storage -p app_manager -p rcoder --all-features` 退出 0；`cargo fmt --all -- --check` 退出 0。未运行测试（阶段约束），未提交未推送。

**R5 完成边界说明**：本批覆盖交接定义的核心场景（`traffic_wake_observing` 无 flag 的旧记录）；旧记录若停在更早 step（如 `traffic_wake_target`，崩溃于写入前后）属未知写窗口，维持保护语义不变——若现场存在此类记录需按 R2 同款证据路径逐案核验，部署后重读现场再定。

### 2026-09-20 深夜五批：R3 设计定案（实现于下批）

本批只读代码定设计，未改代码。R3=prod 物理接管入口，镜像 builder adoption（adoption.rs：操作受理→运行时捕获核验→读栏栅释放→SQL 原子绑定）。

**关键设计约束（从代码实证）**：

1. **Docker 容器 label 创建后不可变**——接管"旧生命周期标签的存活容器"无法靠补 label 完成。而 prod 捕获两侧都做 `validate_application_metadata`（K8s=capture_owned_app_identity 注解校验，"requires lifecycle adoption" 报错点；Docker=capture_stop_target label 校验）。⇒ **Docker 接管必须走 binding 感知捕获**（照 builder 的 `capture_bound_builder_control(context, binding)` 模式：rcoder 层先按名取 UID→查 store binding→传给捕获方法，label 校验失败时以 binding 兜底）。
2. **K8s 注解可变**——接管可在 UID+resourceVersion 前置条件下把当前 context 的身份注解（rcoder.io/application-id/lifecycle-id 等）盖到被接管 Deployment 上（接管即登记动作），随后全部既有 prod 路径无需改动即可通过。
3. 接管核验内容（提交前实时）：名字==app_deployment_name、UID==请求 expected_uid、非删除中、rcoder 家族身份存在（label/注解带 application-id 或 managed-by，旧生命周期值允许不匹配——正是要接管的）、K8s PVC 链（模板 volumes 引用的 PVC 存在且 UID 记为见证）/Docker bind 见证。多候选/墓碑/身份冲突→拒绝。
4. 交付面：shared_types `AdoptApplicationRequest{lifecycle_id,request_id,expected_resource_uid}` + 操作 kind `AdoptApplication`；trait `capture_app_adoption` / `bind_app_adoption`（Docker bind=仅 store 绑定+显式语义）；rcoder 层 `capture_bound_app_target`（binding 感知，替换 compute_control 三处/wake/app_manager 控制流的 prod 捕获入口）+ HTTP `POST /api/v1/userapp/{app_id}/prod/adopt` + resume + OpenAPI。

**顺序依据**：R4（converging 终结链）不依赖 R3，若需要可先做 R4 再回 R3。

### 2026-09-21 凌晨：R3 prod 物理接管入口（按五批设计实施）

**入口**：`POST /api/v1/userapp/{app_id}/prod/adopt`（`AdoptApplicationRequest{lifecycle_id,request_id,expected_resource_uid}`，操作 kind `AdoptApplication`/wire `adopt_application`，scope=Prod）。受理/幂等重试/claim/失败收束镜像 builder adoption（app_adoption.rs）：Succeeded 重放返回原结果、Pending 由恢复扫描/retry 分派续行（recovery.rs+retry.rs 已接）、SQL 提交未知→RecoveryRequired。

**核验链（提交前实时）**：K8s=k8s_app_adoption.rs——Deployment 名字==派生名、UID==请求 expected、非删除中、家族身份（managed-by label 或 rcoder.io/application-id 注解；**旧生命周期值允许不匹配**——重绑正是目的；**异应用拒绝**）、模板引用 PVC 存在且非删除中且无冲突应用注解（UID 记为见证，必须非空）。Docker=docker_app_runtime.rs——容器名/ID/家族 label 同构 + bind 见证。缺席或不可接管→None→干净拒绝，绝不按名接管。

**登记**：K8s `bind_app_adoption` 在已核验 UID+resourceVersion 前置条件下把当前 context 身份注解（+adopted-by-operation）盖到 Deployment——接管即登记，随后**全部既有 prod 路径零改动即通过**（盖章后用严格捕获复核验证）。Docker label 创建后不可变→登记=store binding（`UserAppResourceBinding`，service_type=Userapp）。

**binding 感知捕获**：`binding.validate` 放宽为家族参数化（调用方按 (service_type,uid) 查 key，不可能交叉）；新 trait `adopted_app_physical_uid`（名字→UID 探针，无生命周期校验）+ `capture_bound_app_control`（binding 匹配观测 UID 即授权捕获）；rcoder 层 `capture_bound_app_target`（严格捕获 Conflict 时按 binding 兜底，其他错误原样传播）。compute_control 全部 7 处 prod 捕获点已切换（recover_confirmed before/after、执行捕获、重启再捕获、就绪观察 actual/after、resume 路径）。

**明确剩余**：app_manager 侧 12 处 `capture_app_mutation_target` 调用点（wake/policy/ops/deploy_control/database_preparation/recovery）未切 binding 感知——K8s 接管资源不受影响（注解已盖章）；Docker 接管资源的这些旧路径仍会拒绝，属下批收尾项。

**验证**：`cargo check -p rcoder -p docker_manager -p rcoder-storage --all-features` 与默认 features 退出 0；`cargo fmt --all -- --check` 退出 0。未运行测试（阶段约束），未提交未推送。

### 2026-09-21 凌晨二批：R4 hot_execution 成功侧收束 + converging 终结链（codex 断点续写）

**成功侧收束**（reconcile_hot_failure 原 `phase != Failed → return Ok(None)` 锁死）：

- 观察逻辑抽取为 `observe_hot_owner`（租约身份核验+回执校验+物理目标 exec 读 owner `/v1/deploy/status`+证据构造），仅过滤非终态相位（Deploying/Orchestrating/Idle → None 继续观察）。
- 证据校验按结局参数化：`validate`（checkpoint phase=submit + owner phase=Failed，原语义不动）+ 新 `validate_success`（phase∈{submit,converging} + owner phase=Running，身份链/协议/release_id/持久化等其余条件全同）。
- 分支：owner Failed → 原 `finalize_observed_hot_failure`；owner Running（本操作精确身份）→ 新存储 `finalize_observed_hot_success`（全记录 CAS，step∈{hot_execution,hot_converging}，step=hot_execution_succeeded，证据入 checkpoint `hot_success_observed`）→ 既有租约释放路径收束。"平台侧响应丢失但 owner 已成功"不再锁死。

**converging 终结链**（begin_hot_convergence 与 ConfigMap 写之间的崩溃窗口）：

- `begin_hot_convergence(&converge_env)`：收敛目标 env 先入检查点（`hot_execution.converge_env`）再进 converging 相位——恢复观察者有可比对期望；调用点同步重排（env 构建提前）。
- 新 `reconcile_hot_convergence`（恢复入口已接 retry_control_operation 与 resume_pending_control）：证据序=① `app_env_snapshot` 读回与收敛目标全等（K8s ConfigMap 收敛完成证明）→ 成功终结；② owner 终态 Running（`validate_success`，Docker 唯一来源——env 不可变）→ 成功终结；③ owner 报 Failed（converging 已开始后矛盾证据）→ Conflict 人工核验；④ 未收敛+owner 在途 → 保持保护。

**验证**：`cargo check -p app_manager -p rcoder-storage -p shared_types -p rcoder --all-features` 与默认 features 退出 0；`cargo fmt --all -- --check` 退出 0。未运行测试（阶段约束），未提交未推送。R4 两侧（失败侧既有+成功侧本批+converging 链本批）至此齐；Qoder 复核遗留的"编排中 Stop 后 Start 报 explicit redeployment required"（server_journal resume 保护）不在本批范围，属 app-cli 侧恢复协议，已在前交接记录。

### 2026-09-21 凌晨三批：R6 文档对齐（java-compute-control.md 补 Docker 分支与新恢复能力）

- 原清单只列 K8s 分支（审计确认的文档落后于实现）：补 Docker dev Restart 归档重建（auto-remove 语义）/Docker Stop 回执/Docker prod 边界（重启归档暂不支持的理由）/两后端的创建承诺采纳。
- 补四项新恢复入口：旧版本 wake 锁证据收束、hot_execution 成功侧、hot_converging 终结链、prod 物理接管端点（含 K8s 盖章与 Docker binding 两种登记语义）。
- 保留"没有匹配回执的旧版本操作仍要求进一步核验"的边界声明与未联调声明；历史条目未改写。
- tasks.md 勾选状态同步未在本批完成（追加式历史文档，需按实现逐项核对后更新，防止照抄过早结论）——R6 剩余小项。

compute-control 线功能开发至此：F1、R1、R2a、R2b/N1、R5、R3、R4、R6(主体) 全部落地（编译+fmt 验证）；明确剩余：R6 尾（tasks.md 同步）、R3 尾（app_manager 12 处捕获点 binding 感知）、Docker prod 重启归档（凭据重注入通道）、全部测试/部署验收。

### 2026-09-21：Qoder 遗留——编排中 Stop 后 Start 无法恢复业务（app-cli）

**缺口**：切换已确认的部署被打断后，journal 归和链最终落到 `StartupFailed`（含已确认 active 制品身份）；启动路径对 StartupFailed 一律停在 ServerPhase::Failed，而平台对 Failed 相位的 app-cli 没有任何"启动业务"命令通道——只有整轮再部署能拉起业务。spec 明确"Stop 完成后用户重新发起的新 Start 正常受理"。

**修复**（server.rs initialize_startup）：StartupFailed 且制品身份与迁移确认（上游既有核验）时，经 `require_fresh_process_scope` 护栏（前编排进程组确已退出=容器重启后的显式新尝试，非进程内重试循环）→ 按已确认制品走 Existing 编排恢复业务。历史操作结果保持 Failed 不改写。防循环：进程存活期内不再重试（init 一次）；容器重启频率由平台重启策略约束。

**验证**：app-cli 独立 `cargo check --all-features` 退出 0；fmt 通过。未运行测试（阶段约束）。Qoder 报告的另两个失败断言（改密前后业务可用）预计随业务可恢复而闭环，e2e 阶段验证。

### 2026-09-21：测试前检查（用例时效评估，未新增场景）

- 过期断言扫描：tests-e2e 无对 "Original restart template"/"Production identity changed"/"Restart target changed" 的字符串断言——R1/R3 改动的报错文案无测试耦合。
- 新行为覆盖评估：Docker 每次 dev restart 即走 R1 归档路径（auto-remove 语义），现有 dev restart 场景隐式全覆盖；deploy_full_chain 的 3 个历史失败断言（Qoder R1）即 app-cli 修复的验收；R2a/R4/R5 属故障注入窗口，宜工具脚本（manual_owner_stop.py 模式）在测试阶段按需补。按用户"高价值不滥加"约束，本轮不新增默认套件场景。
- e2e 前置：dev-hot 重建 rcoder 容器 + docker-build-agent-runner + docker-build-app-runtime（app-cli 修复在这两个镜像内，qoder 报告已证明旧 app-runtime 镜像会掩盖此类修复）。

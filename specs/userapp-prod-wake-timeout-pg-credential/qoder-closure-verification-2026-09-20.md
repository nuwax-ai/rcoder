# Qoder 收尾验证记录（2026-09-20 第二轮）

接手基线：实现 `c1f8f584`，交接文档 `6a943fcf`。本轮源码头 `ff11298f`（feature-userapp）。
组件、真实 PG、跨机 native、Compose、K8s、发布分开报告；旧报告原基线不改写。

## P0-1 显式部署携带 pg 的未知结果恢复——缺口核对（结论）

- 显式部署改密原先走 `align_pg_credentials_with_admin`（verify→ALTER→re-verify），无事务回执：断连/超时/Applied 后 checkpoint 提交失败三类时序的 SQL 结果均不可证明。
- `execute_deploy_input` 的重放守卫只认 `explicit_pg_target` 拒绝，无恢复入口；`OwnedOperation::fail` 在 checkpoint 非空时落 RecoveryRequired 并持有 prod 槽位（围栏正确，但无解围通道）。
- `wait_deploy_stage`（业务等待）在 pg 对齐之前：业务带新密码不 Ready 时改密永不执行（顺序缺陷）。SQL 阶段用本地 trust 管理通道（`pg_wait_ready_cmd` 以 `$POSTGRES_USER` socket 连接），与业务密码无关，顺序调整不影响 SQL。

## P0-2 恢复闭环实现（提交 0124d3b4）

1. 部署改密切换为回执事务：`password_operation_command`（密码变更+回执同事务提交），checkpoint 记 `ExplicitDeploymentPasswordEvidence`（receipt_protocol=1，write_submitted→verified）。断连/回执缺失/复验失败分别按 Unknown/AppliedButUnverified 传播。
2. 新恢复入口 `POST /api/v1/userapp/deploy-pg/recover`：原 operation/lifecycle/物理身份/输入；取消墓碑或 committed+TCP 验证为仅有的两种证明；物理目标被替换/legacy 证据/过期快照/异身份拒绝。两种结果终局 Failed（部署完成证据未记录），receipt 结果留在 checkpoint 可查询。旧无回执写拒绝自动恢复。
3. 顺序修正：pg 对齐移到业务等待之前（管理通道不依赖业务 Service Ready）。
4. 存储 `finalize_deploy_pg_recovery`：应用根 CAS 短事务，镜像 `finalize_password_recovery` 的证据校验与槽位/租约规则。

### 组件验证（nextest）

| 命令 | 退出码 | 结果 |
|---|---|---|
| `cargo nextest run -p shared_types -p app_manager -p rcoder --no-fail-fast` | 0 | 840/840 |
| `cargo nextest run -p rcoder-storage --features userapp-turso --no-fail-fast` | 0 | 141/141 |
| `cargo clippy -p shared_types -p app_manager -p rcoder --all-targets` | 0 | 无警告 |
| `cargo clippy -p rcoder-storage --all-targets --features userapp-turso` | 0 | 无警告 |
| `cargo fmt --all -- --check` | 0 | 通过 |

新增反例：`explicit_deployment_write_failure_keeps_write_submitted_evidence`（写失败→Unknown+write_submitted 证据）、`explicit_deployment_missing_receipt_stays_unknown`、`explicit_deployment_tcp_failure_is_applied_but_unverified`、`deploy_pg_recovery_finalization_is_cas_identity_and_outcome_bound`（缺租约/legacy/换目标/过期快照/在位围栏/幂等终局）、`deploy_pg_snapshot_*`（身份/终局一致性/legacy/异 kind 拒绝）。既有 happy 用例更新为回执断言（BEGIN/COMMIT/捕获管理员身份、无密码泄漏）。

## P0-3 reset-password 端到端（提交 ff11298f）

新增 `verify_password_governance_after_change`（compose_userapp_deploy 主链，紧随 immediate 检查）：同 request_id 重放幂等、停止态改密拒绝且不隐式唤醒容器、显式 start 后新密码保留（不回写历史保存值）、restart 带 pg 对齐后对已完成部署调 deploy-pg/recover 必须拒绝。contracts.py 严格清单同步 +12 项（CR10 共 31 项）。`cargo check -p rcoder-e2e --tests` 退出码 0。
运行验证依赖重建后的 Compose 镜像（见 P3）；未运行前不宣称通过。

## P1 native guardian/retire

真实 binary 十项脚本 `crates/file-server-proxy/tools/test_native_guardian.py`：

| 平台 | binary sha256（前 16） | 结果 | 报告 |
|---|---|---|---|
| macOS arm64（本机） | 8966b632aca31124 | 10/10，退出码 0 | tests-e2e/reports/qoder-native-guardian-mac-2026-09-20.json |
| Linux x86_64（192.168.32.131，源码 git bundle 增量拉齐至 ff11298f 后原生构建，56.58s） | 883de5d8390e91bc | 10/10，退出码 0 | tests-e2e/reports/qoder-native-guardian-linux-2026-09-20.json |

分发方式经用户确认：`git bundle create /tmp/rcoder-qoder-head.bundle 81b8a480..HEAD`（3.2M，仅已提交对象）+ scp + 远端 `git fetch`/checkout `qoder-verify` 分支。
Windows .53 未执行（见未完成项）。

## P2 数据库合约

- 旧 SQLx 残留：`grep -rln sqlx crates/rcoder-storage/src` 为空。
- 退役接口消费者：`align_pg_credentials` 仅剩 dev 语义调用点（runtime/db.rs）；`align_pg_credentials_with_admin` 无生产调用（导出保留给既有契约测试）。
- 真实 PG 契约复跑（本轮 storage 改动后）：在 .131 e2e-pg 容器内新建独占可破坏库 `rcoder_qoder_verify`（owner app，不触碰业务库），`RCODER_PG_TEST_DSN` 指向该库运行 `cargo nextest run -p rcoder-storage --features pg --no-fail-fast`（源码头 ff11298f）：
  - 退出码 0；`85 tests run: 85 passed, 12 skipped`（12 项为环境门控，不冒充通过）。
  - 含 ProjectStore 全套（含 leader_election_mutual_exclusion 11.8s 真实互斥）与 userapp PG 主契约（S01-S04/S07-S12 各 PG 入口）。
  - 测试库保留供后续复跑；未触碰 e2e-pg 业务库。
- app-cli 独立检查（本轮 shared_types additive 改动后的编译面）：
  - `cargo fmt --manifest-path crates/app-cli/Cargo.toml -- --check` 退出码 0；
  - `cargo clippy --manifest-path crates/app-cli/Cargo.toml --all-targets` 退出码 0、无警告；
  - `cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast --all-features` 退出码 0，`264 passed, 1 skipped`。

## 未完成项 / 受阻

1. **Windows .53 native 验证受阻**：机器在线且工具链齐（cargo/rustc/Python 3.12），但本地无仓库 clone；全量 git bundle 318M 过大，纯源码 tar 因仓库内未识别的大目录（排除 target 后仍 >2.1G）不可行。且 `test_native_guardian.py` 为 Unix 场景（SIGKILL/pipe 语义），Windows 需按 native tasks 的 Windows 专项（Job/控制台/进程树）单独设计与执行——待用户安排分发方式后进行。
2. **Compose E2E（两轮，报告 c56c38f8 / f862b170）**：`make dev-build` 退出码 0（dev-master-rcoder:latest digest `d81c33975ec078a7…`）；`make dev-restart` 后旧开发数据卷触发 Toasty 基线保护，`docker/data/rcoder/{userapp.turso.db,userapp.turso.db-wal,userapp.sqlite3}` 可逆移入 `legacy-backup-20260920/` 后容器 healthy。二进制身份：容器内 `/app/bin/rcoder` sha256 `492e1416a279e54f…`；`deploy-pg/recover` 空 body 探测返回 `ERR_VALIDATION`、不存在操作返回 `ERR_NOT_FOUND`+operation_id（新路由在运行二进制中激活）。**主链两轮 56 断言全绿**（构建/部署/观测/文件/失败镜像保护/热部署/轻量部署/stop-wake——pg 对齐前移无主链回归）。**CR10 preconditions 两轮稳定失败**（ok=false：唤醒后 `Application runtime not found`（activity_registry::wake 探测）→ proxy backend 未注册 → React 探测 180s 超时；reset-password 主体未执行，governance 族连带跳过）——待归因，候选：全新 Turso 库首次走 stop→wake 重建链 / 共享环境并行干扰；本轮前移改动不经此路径（该部署不带 pg）。**"source changed during run"归因闭合**：并行会话在运行期间修改 `crates/rcoder-storage/src/db/*` 并新增 schema/specs 文件（`git status` 实证），首轮叠加我在运行中写验证记录；reports/ 与 docker/data/ 均在 .gitignore（check-ignore 验证），非漂移源。两轮整体 fail 由漂移保护标记，非断言工具问题。
3. **dev-app-runtime 镜像**：本轮 shared_types 改动为 additive（app-cli 行为等价），镜像 6 小时前构建；计划在主链 E2E 前重建并核验 digest。
4. **P4 个人 remote K8s 受阻（环境故障，非业务失败）**：`make remote-k8s-doctor` 失败，`ssh exited 1: Unable to connect to the server: net/http: TLS handshake timeout`。诊断：k3s 双节点集群，控制面在 192.168.32.226（swufe-x10dai，未列入本人可操作机器）；k3s-agent 日志显示到 `192.168.32.226:6443` 的负载均衡健康检查反复 `FAILED*->RECOVERING->ACTIVE`，.131 本机 kubectl（127.0.0.1:6444）请求超时；worker 节点 `soddy` NotReady 19 天，`k3s-agent` 服务 active 但租约续期失败。控制面机器网络/服务不稳定属环境阻塞，未操作 .226；恢复后按 P4 清单执行（verify smoke → userapp/chat/gateway + PG 原地改密双副本补验）。
5. **Java 联调**：deploy-pg/recover 与 reset-password 治理族 OpenAPI 已同步；Java 侧对接未执行，保持未完成状态。
6. **发布**：按交接 P5 归用户后续安排，本轮无 push/tag/发布。

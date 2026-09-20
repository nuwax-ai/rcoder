# Qoder 收尾交接：开发缺口、组件与真实部署验收

更新日期：2026-09-20。**这是当前执行清单，替代本文件旧版。不得继续实施已撤销的 source-seal、改密换代和待生效配置方案。**

## 1. 基线与执行边界

- RCoder：`/Users/soddy/Documents/git-workspace/rcoder`，分支 `feature-userapp`。
- 最新实现提交：`c1f8f584`，`fix(userapp): apply database passwords in place`；其父提交 `f5755820` 是上一轮 native/离线交接阶段保存。包含本交接文档的后续 docs 提交仅更新交接与规范。
- 本轮已保存代码；没有 push、npm 发布、镜像发布或部署。接手先核对实际 HEAD、工作树、相关 diff 及当前运行的测试进程，保留无关修改。
- 本轮目标为完成剩余实现并验证。不要将历史通过结果当成当前二进制通过。组件、真实 PG、Compose、K8s、三平台、发布分别报告。
- 尽量集中修改、集中编译；失败后只重跑相关范围。不要让 Cargo 共用 target 并发执行，也不要在同一环境测试过程中替换部署。

配套仓库实查：

| 仓库 | 状态与边界 |
|---|---|
| `build-agent-docker` | HEAD `fb12fa6`；Chart.yaml、VERSION 有用户未提交改动，必须保留；本次没有提交这两项 |
| `userapp-workspace-template` | 本轮核对工作树干净；用于真实模板构建验证，不无理由扩大修改 |

## 2. 不得反向改变的最新需求

**立即修改当前 PG 的密码，应用重启由用户决定。**

1. 使用 `POST /api/v1/userapp/db/{app_stage}/reset-password`。运行账号也可改密，dev/prod 按 app_id + lifecycle + scope 和实际资源身份隔离。
2. 不为改密停止业务、不主动终止数据库连接、不重建容器/Pod、不创建 helper、不重新挂载 RBD。
3. 已认证 PG 会话继续执行；新建/重连的密码认证连接使用新密码。业务连接配置更新和是否重启由用户决定，不能宣称连接池以后一直正常。
4. prod 容器未运行时明确拒绝改密，用户先显式启动；不自动唤醒业务。dev 原有 builder 准备行为未在本轮扩改。
5. 普通启动、重启、热部署、自动唤醒不应用历史保存密码。显式部署请求自己的 `pg` 输入仍是独立的明确用户意图。
6. 开发期新增的 prod `runtime-configuration` 保存/查询 HTTP 路由已退役，不恢复“保存待下次生效”。保留历史存储结构不代表还允许启动时消费它。
7. 保留真正的未知 SQL、目录切换、迁移与资源清理保护；不能删除 journal、租约或原操作来制造成功。已有 PGDATA 不用默认密码覆盖。

必读：

- [原地改密方案](../userapp-prod-wake-timeout-pg-credential/immediate-password-update-2026-09-20.md)
- [本轮验证记录](../userapp-prod-wake-timeout-pg-credential/immediate-password-update-verification-2026-09-20.md)
- [数据库回执恢复协议](../userapp-prod-wake-timeout-pg-credential/password-recovery-receipts.md)
- [Java 接口交接](../userapp-prod-wake-timeout-pg-credential/java-runtime-configuration-handoff.md)

## 3. 已有证据与验证缺口

| 范围 | 实际结果 | 边界 |
|---|---|---|
| app-cli | 264/264 运行项通过，1 跳过；Clippy 通过 | 未代替三平台和容器实测 |
| 受影响六个 crate 全 features | 1230 运行，1226 通过，4 个配置夹具失败；修复夹具后相关 18/18 全部通过 | 不是修复后再次运行整个 workspace；18 个原有跳过不能算真实 PG 通过 |
| 默认 Docker features 聚焦 | 9/9；294 筛选排除 | 不是完整默认 features 回归 |
| 受影响模块及 rcoder-e2e Clippy | all-targets/all-features 通过，无 warning | E2E 仅编译，未运行 |
| fmt/diff | 根 workspace、app-cli fmt check 和 diff check 通过 | 文档与源码检查 |
| 历史 PG 合约 | 旧轮独立 PG 31/31 | 不代表当前代码的双副本 K8s 验收 |
| 历史 Compose | 旧完整轮 34/44；之后有局部修复 | 当前镜像未重建、未重新完整执行 |
| native guardian/retire | 当前 process_utils 组件及 proxy 编译已覆盖；此前真实旧 binary 7 项通过 | 最新十项真实进程脚本、三平台尚未验收 |

日志与退出码见本轮验证记录。`/tmp` 证据接手先检查是否存在，需留档时提取脱敏摘要。不要复制私有环境、密码或 owner token。

## 4. P0：补齐改密与恢复的实际业务闭环

### 4.1 显式部署携带 pg 的恢复缺口（需要开发确认）

源码入口：

- `crates/app_manager/src/lifecycle/deploy_control.rs::execute_deploy_input`
- `crates/app_manager/src/runtime/runtime_configuration.rs::apply_explicit_deployment_credentials`
- `crates/rcoder/src/userapp_forward/db_password.rs` 与 `db_password/recovery.rs`
- `crates/shared_types/src/pg_password_receipt.sql`、`pg_utils.rs`

当前显式部署只对现有物理目标原地改密，记录 `explicit_pg_applying` / `explicit_pg_applied`。重放路径只要 checkpoint 含 `explicit_pg_target` 就拒绝继续；这保护了未知写，但**不能证明已有可用恢复闭环**，并且需要区分已确认 Applied 与 Applying 未知。

必须处理：

- 追踪改密成功后控制记录提交失败、PG 执行断连、已记录 Applied 后协调器中断三个时序；通过真实调用链反例验证。
- 选择复用改密事务回执/原操作恢复，或其他能证明原 SQL 结果的简洁方式；保持原 operation/lifecycle/物理身份及输入，不用换 ID 盲目重做整次部署。
- 已确认 Applied 不再次改密；未知不能仅凭当前密码登录成功就放行，因为迟到写仍可能存在。
- 明确恢复入口、可查询状态与调用说明。不要通过恢复 source-seal、换代、待生效配置或简单移除重放保护解决。
- 核对 deploy 的业务等待和 PG 对齐先后顺序，避免业务因错误密码不 Ready，又必须等业务 Ready 才能改密。管理通道和 PG 就绪不依赖业务 Service Ready。

### 4.2 当前 reset-password 真正端到端验收

现有 Compose 用例：`tests-e2e/tests/compose_userapp_deploy.rs::verify_immediate_runtime_password`；严格清单 `tests-e2e/tools/contracts.py` 已同步十项 CR10 断言。

新增用例已经包含：同一 psql TCP 会话改密前后 backend PID 相同、新密码新连接成功、旧密码新连接失败、prod UID/owner 不变、dev UID/密码不变、真实业务代理可用。当前测试放在普通 stop/wake 检查之后，避免用旧业务连接配置重启造成测试干扰；**这一排序不证明重启后不会覆盖密码**。

还需补验：

- 当前 prod 停止时改密明确失败，既不启动容器也不启动业务。
- 用户更新业务连接配置并显式重启后，新密码保留，不重新写历史保存值；区分用户发起的业务重启与接口自动重启。
- SQL 提交/响应丢失、协调器退出、同请求重放、并发另一改密、同 app dev/prod 隔离、原物理目标被替换、recover 原身份。
- 新旧密码连接验证必须实际采用密码认证，不能由 trust 掩盖断言；已有 TCP 必须已认证，不能用两次新连接替代长连接。
- 同步 OpenAPI 和 Java 联调结果；未联调就保留未完成状态。

## 5. P1：native guardian/retire 与两组件三平台

重点文件：`crates/process_utils/src/guardian.rs`、`crates/file-server-proxy/src/{main,native_control,native_supervisor}.rs`、npm 入口、`crates/file-server-userapp/src/handlers/userapp_dev.rs`。

- 最新 guardian 摘要格式化编译问题已在 c1f8f584 修复，不重复修。
- `RetirementAccepted` 只代表受理；只有原 supervisor 对原 owner 的真实 wait 见证才是 `OwnerExited`。不得伪写 Stopped/Quiescent 或假冒 app-cli 成功。
- 验证错误 token/instance、响应丢失原实例重试、Pending 与 stop 交错、spec 摘要不匹配、guardian 自身退出后的未知保护。
- 查阅 [native tasks](../native-desktop-runtime/tasks.md) 与 `nd06-07-control-completion-2026-09-20.md`，按 ND01–ND12 / NT01–NT16记录；不能把当前 process_utils 测试当作完整 proxy/native 验收。

按需要补齐组件后，重建真实 binary 再执行：

```bash
cargo build -p file-server-proxy --bin file-server-proxy
python3 crates/file-server-proxy/tools/test_native_guardian.py --binary target/debug/file-server-proxy --report /tmp/rcoder-qoder-native-guardian.json
```

原脚本是 Unix 场景；Windows 必须验证自己的 Job/控制台/进程树实际行为。macOS、Linux、Windows 都测试 app-cli 与 file-server-proxy：真实模板构建、重复启动同 owner、显式 restart/stop、外部占用 3010、子树退出、直接 binary/npm 两种入口、只读/离线完整包、Pingap 版本及 OS/arch/ABI 校验。

用户提供的个人测试机器允许安装/更新依赖；地址与登录方式从已有私有配置/SSH 读取，不将账号密码抄入文档。原生模式不依赖容器或外置 supervisor 服务；不深入 Electron 本体、不默认禁用 public bind。

## 6. P2：Toasty/schema/CAS 与全仓组件收口

阅读 `specs/toasty-storage-unification/{tasks,schema-design,acceptance-matrix-2026-09-20,verification}.md`。

- 核对 S01–S12；复用有明确基线的证据，补真实 PG/Turso 极值、NULL、非法持久行、时间/JSON 边界，不能以 codec 单测替代数据库读写。
- 核对旧 SQLx/旧表、退役接口和配套消费者；历史配置表仍保留，不擅自清库或删除未知操作数据。
- 跨副本受理、删除/重建、槽位与请求幂等继续使用共同 CAS 竞争点和短事务；不得重新依赖 SELECT FOR UPDATE 或进程锁维护多副本正确性。
- PG 并发用独立连接验证；数据库事务超时/取消、关机、连接归还和断连保护按已有矩阵收口。
- 保留当前 PG/kubernetes feature 限制，不扩展 Compose PG 多副本需求。
- 最终按 AGENTS.md 补根 workspace 默认/全 features、app-cli 独立检查；本轮仅受影响范围通过，不能写全仓最新全部通过。禁止 hotpath-alloc。

## 7. P3：先重建正确制品，再完整 Compose

历史报告：`tests-e2e/reports/6b0904fb0cf745f095b0e39952fdc006/` 为34/44；8项缺真实 LLM 私有配置、two_users 隔离修改待复跑、deploy_full_chain 原 CR10 失败报告 `dd511bc95c1a4f6cbaa5ab3d95d0571b`。这些是旧证据，不能归因为当前仍同一故障，也不能修改旧报告为成功。

已核对 Make 行为：

- `make dev-hot` 重编主 rcoder 并 restart，不自动更新已有动态容器的 app-cli/proxy。
- `make dev-restart` 先 dev-build 再重建 Compose 服务，不代表动态 UserApp 全部更新。
- 本轮改动 app-cli/shared runtime，须检查 `make docker-build-agent-runner`、`make docker-build-app-runtime` 的当前构建输入并重建需要的镜像。
- 镜像构建成功后核验实际运行容器中的 binary SHA、镜像 digest、Pingap 配对；不能用相同版本号或 latest 标签代替。不要在容器里无意义重复 npm 安装。

```bash
# 先读 make/dev.mk、make/docker.mk 与实际 Compose 配置，选对构建/更新入口
make docker-build-agent-runner
make docker-build-app-runtime
# 按当前部署选择 make dev-hot 或 make dev-restart
make test-e2e E2E_SUITE=compose_userapp_deploy E2E_FILTER=userapp_deploy_full_chain
# 修复相关失败后再运行完整套件
make test-e2e
```

真实 AI 测试配置缺失必须明确补齐或记录受阻，不伪造响应、不跳过后算通过。只清理本轮明确拥有的资源，不批量删除历史容器/卷。整轮报告要绑定源码和实际镜像，覆盖为空不能算通过。

## 8. P4：个人 K8s/PG 和真实业务

按 [remote-k8s README](../remote-k8s-dev/README.md) 与 `.env.local` 核对个人 `.131` 双节点环境，不操作此前故障现场。

- 历史环境 ID `c188d7e8de407557`、namespace `rcoder-e2e-soddy` 只是定位线索，接手重新确认。与 Helm `nuwax-k8s-test` 分开记录。
- 个人 PG 重建虽已有授权，也先核对目标库独占归属、旧写入者和资源身份；这是基线切换，不能每逢失败就清库。agent PVC 永不删除，不删共享卷根或整个 namespace。
- 旧构建 `20260920T002721Z-798ba6d3` 不是当前镜像证明。

```bash
make remote-k8s-doctor
make remote-k8s-verify SUITE=smoke
make remote-k8s-test SUITE=userapp
make remote-k8s-test SUITE=chat
make remote-k8s-test SUITE=gateway
```

同一源码快照追加 suite 无须重复构建，源码改动后重新 verify。补验当前 PG 原地改密的同 Pod UID/RBD 不重新挂载、两个 RCoder 副本并发、断连恢复、持久化重启、关机、删除租约恢复、Preview/SSE 与动态 UserApp Gateway 真实路径。smoke 或健康接口不代替业务验收。

## 9. P5：发布与交付

先完成开发及验收报告。发布是单独阶段，按用户后续安排处理，不因本次 commit 指令自动 push、打 tag 或发布。

- app-cli/file-server-proxy：版本、提交、tag、CI、npm 主包与平台包真实安装分别验证。
- build-agent-docker 配对核验 PG 管理脚本、工具链和当前组件版本；保留用户 Chart/VERSION 修改。
- 后续用户安排镜像发布时使用：

```bash
make setup k8s-helm-rcoder-version-publish ENV=test AMD64_ONLY=1
```

用户明确希望 Helm `--reset-values` 使用新 Chart 默认值，不擅自改成沿用旧 values；选择实际发布版本、核验集群/namespace，再记录部署与业务结果。

## 10. 完成标准与交付报告

每项标明：已实现 / 组件通过 / 真实环境通过 / 未验证或受阻。列源码提交、实际命令和退出码、报告路径、失败根因及复跑。发现测试偏移须根据最新需求修正并保留行为断言；代码错误修代码。不得降低覆盖、删除有效断言、换旧镜像或清锁来得到绿色结果。

本轮后续验收结果另写日期化记录；保留旧报告原基线。最后给出仍有无发布阻断项，不用“应该没问题”替代证据。

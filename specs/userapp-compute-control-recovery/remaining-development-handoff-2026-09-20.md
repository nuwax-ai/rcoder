# 剩余开发交接（2026-09-20）

## 1. 结论与接手基线

**当前不是全部开发完成，也不是只剩测试。** 高优先级停止/重启、旧资源恢复已有大量生产实现，但下面列出的恢复分支与凭据恢复仍未闭环。不要因编译通过就交付整个功能。

- 仓库：`/Users/soddy/Documents/git-workspace/rcoder`
- 当前分支：`feature-userapp`；HEAD：`a571a0ea`。
- 实现主要在大量已修改和未跟踪文件中，**仅 checkout HEAD 无法得到本轮成果**。接手必须保留整个现有工作树，尤其新增 compute、discovery、receipt 模块和 schema 文件。
- 本文按当前交接状态汇总；`tasks.md`、`verification.md` 是追加式历史记录，早期“尚未完成”有些已被后续实现覆盖，不能只读文件开头判断现状。
- 本轮仅整理交接，不提交、不推送、不部署、不修改现场。

必读同目录 `spec.md`、`plan.md`、`tasks.md`、`verification.md`、`java-compute-control.md`，以及根 `AGENTS.md`。

### 最新接手摘要（本次重新核对）

**先做 R4 的平台调用闭环，再推进 R1–R3、R5，最后对齐 R6。** R1–R6 是未闭环的功能范围，不是仅六处小修改；不承诺未经验证的剩余工时。

本次已重新核对分支、工作树以及实际部署入口。其余缺口沿用逐批实现记录，接手时需逐分支复核；本文不是重新完成了一次全仓审计。

| 类别 | 当前状态 | 接手动作 |
|---|---|---|
| 平台向 owner 补交凭据 | app-cli 新 runtime 入口部分支持，平台热部署仍走旧入口 | 优先接通实际链路，见第 11 节 |
| Restart 缺失资源恢复 | K8s dev 捕获归档后的分支已实现；其余不完整 | R1 |
| 远端写与回执间崩溃 | 部分回执和核验已实现 | R2 |
| 历史资源自动登记 | 有标记、可核验分支部分实现 | R3 |
| 旧 wake 操作恢复 | 新确认点支持有限恢复，旧记录未完整覆盖 | R5 |
| 接口及所有调用方一致性 | 已有接口与文档，仍需最后闭环 | R6 |
| 集成、三平台、发布 | 本轮未完成 | 功能完成后单独验证，不算待开发逻辑已通过 |

## 2. 用户已确认的边界（不能重新设计偏移）

1. Stop > Restart > 普通业务操作。普通业务占槽、RecoveryRequired 不能直接阻止容器控制受理，但实际执行要收束旧写入。
2. **不做内部请求排队。** Stop 执行期间新 Start/Restart 返回明确冲突；Stop 完成后可显式启动或重启。Stop 可以接管进行中的 Restart。
3. dev/prod 独立；容器控制保留 PVC 和数据；应用删除墓碑不得被恢复撤销。
4. prod Restart 恢复已确认版本，不隐式发布，不重跑结果未知的迁移。
5. **修改 PG 密码立即作用于当前 prod 的 PG，是否重启业务由用户决定。** 不为改密拉新 Pod、做 source-seal 或恢复默认密码。已有 TCP 会话不因改密自动断开，新连接需新凭据。
6. PG 等待是运行环境策略：RCoder 的 UserApp dev/prod 默认注入 `APP_CLI_REQUIRE_PG=1`；独立 app-cli 默认不探测，不能仅因模板有 migrate 就强制探测。显式探测尊重真实数据库地址、端口和凭据，支持远程 PG。
7. 不因本轮任务把 dev STS 改成 Deployment；控制器选型已单独记入 `controller-choice-follow-up.md`，暂不实施。
8. 用户当前要求**先补生产逻辑，可编译，暂不新增或运行测试**。不要在本阶段堆测试；后续得到测试阶段指示后再做第 6 节验收。
9. 不用长事务、SELECT FOR UPDATE 或进程内锁替代多副本 CAS。超时、租约年龄、端口可连、同名 Pod 都不能单独证明旧写入结束。

## 3. 已有实现：继续完善，勿重写或重复开发

以下表示源码已接入，不代表整条链已部署验收：

- compute 控制记录、优先级受理、执行代次、独立租约、Stop/Restart 后台执行、202/操作查询和部分原操作恢复。
- Docker/K8s 启停及 builder 创建/取消回执；Stop 接管后阻止旧业务成功提交；已发出的 runtime future 完整观察，不能丢弃后假装取消完成。
- 旧资源发现、部分生命周期恢复、PVC UID 持久见证；查询不再旁路创建 Service，修复走受理操作。
- agent 自行启动 app-cli 后，由 file-server 验证 owner 并通过 owner API 停服务，避免按进程名清理。已有单场景历史实测，跨目录和完整组合验收不能据此视为完成。
- **K8s dev Restart** 停止前将原 STS 模板与卷身份存入私有不可变 Secret；停止后控制器丢失可先重建零副本 STS，核对原 PVC，再按原操作继续启动。公开 checkpoint 只存引用。
- 成功 Restart 的归档 Secret 按 UID/resourceVersion 清理；最新补丁让无租约的成功记录也进入待清理扫描，并 CAS 标记 `builder_restart_archive_cleaned`，避免永久重复读取。
- wake 冲突 fail-fast，保留结构化 blocker；通过身份绑定的 Pod/container exec 读取部署失败详情，不依赖 Ready Service。
- wake 完整启动写返回后持久化 `traffic_wake_observing` 和 `start_write_acknowledged=true`；其后的只读观察失败可精确终结。符合该证据的 RecoveryRequired 接入扫描与原 retry 入口；**无此证据的旧操作不自动解锁**。
- app-cli 两种编排共用 PG 预检策略及迁移回执；确定性的命令/目录输入错误在写迁移意图前失败。

## 4. 仍需开发的生产功能

### R1：Restart 在缺少原控制器/容器时的恢复（高优先级）

**缺口：** 目前新增归档覆盖 K8s dev“先捕获模板、后停止、其后控制器丢失”。请求刚进入时控制器已消失且没有归档、Docker 和 prod 对等重建路径仍需完成。

入口：
- `crates/rcoder/src/userapp_builder/compute_control.rs`
- `crates/rcoder/src/userapp_builder/recovery/compute.rs`
- `crates/docker_manager/src/runtime/k8s_builder_restart.rs`
- `crates/docker_manager/src/runtime/docker_builder_control.rs`
- `crates/docker_manager/src/runtime/k8s_app_lifecycle.rs`
- `crates/container-runtime-api/src/runtime_trait.rs`

实施要求：从可信持久部署/运行配置和卷绑定恢复模板；无足够来源时返回可查询、可继续的恢复阶段，不能猜默认镜像或按名称选卷。先证明旧计算实例退出，再使用原卷启动；覆盖 RBD。Docker 捕获容器 ID，K8s 条件 UID 写入。归档不能含公开可读密码。

完成条件：三条路径 dev/K8s、dev/Docker、prod 的缺失资源 Restart 均有明确恢复来源和续行流程；不能仅移除 absent 拒绝分支。

### R2：普通创建/绑定恢复及取消的回执前崩溃窗口（高优先级）

**缺口：** 创建或取消已发生，但独立完成回执尚未持久化时进程崩溃，仍有不能自动核验的分支。K8s 最终 Service 回执已覆盖部分窗口，不能外推到前面所有写入、旧绑定恢复及 Docker。

入口：`creation.rs`、`recovery.rs`、`recovery/compute.rs`，以及 runtime 中 `builder_creation_receipt.rs`、`k8s_creation_receipt.rs`、`docker_compute_receipt.rs`、`k8s_agent_create.rs`、`builder_completion.rs`。

实施要求：逐阶段列清 PVC/Service/控制器/容器/start 的写入前身份、运行时可验证回执与确认点；有证据则按原 operation/executor CAS 继续或收束，无证据保持明确恢复阶段。不得只看最后错误是 4xx 就忽略此前副作用，也不得发现容器存在就伪造成功。检查 Stop 抢占、被替代 Starting/Stopping 与迟到写的交叉时序。

完成条件：每个复合写阶段都有原操作恢复出口或具体不可判定原因；控制槽不会因缺一个进程内回调永久卡住，新 Stop 不被旧迟到写再次拉起资源。

### R3：历史无标记资源、孤儿及失败新根与旧资源归并（高优先级）

**缺口：** 无生命周期标记且无历史登记、ownerReference 已丢失的孤儿、仅失败 ensure 新根与旧资源之间的某些归并路径仍未完整实现。

入口：`userapp_builder/adoption.rs`、`recovery/discovery.rs`、runtime `lifecycle_discovery.rs`，storage `common/repo.rs`、`common/ops.rs`、`schema/userapp-recovery-witness-v3.sql`。

实施要求：复用现有发现协调器，提交前实时核对控制器/Pod/PVC UID、挂载关系与管理归属。唯一可核验时原子登记；停止状态保留；多生命周期或删除墓碑不能盲目合并。失败新根只有确认未产生有效新资源后才能恢复旧身份，同时保留失败历史。没有来源证据的资源需要明确恢复入口，不能无限通用报错，也不能随意接管。

完成条件：数据库缺登记后可恢复可核验旧 dev/prod；无标记分支有可落地的绑定策略；PVC 与实际挂载一致，不把 watch/cache 当提交依据。

### R4：app-cli 崩溃后的原操作凭据补交与 hot_execution 恢复（高优先级）

**缺口：** 当前缺少凭据时已保护业务启动，并允许控制循环处理停止，但原操作补交凭据接口/平台调用尚未完整接通；`hot_execution` 的完整核验仍未闭环。

入口：app-cli `api/runtime.rs`、`runtime_kernel.rs`、`server.rs`、`supervisor.rs`；app_manager `runtime/runtime_configuration.rs`、`runtime/database_preparation.rs`、`lifecycle/hot_deploy.rs`；rcoder `userapp_forward/db_password/deploy_recovery.rs`。

实施要求：以原 operation、生命周期、配置版本补交运行凭据，不能用新请求覆盖旧执行身份。平台直接改密后，下次用户显式启动/重启的业务、迁移和两个编排引擎取得一致的新凭据；不自动改回旧密码。曾尝试的额外明文凭据恢复文件方案已撤回，不要恢复该方案。已完成迁移不重跑，未知迁移保留证据和恢复流程。

完成条件：owner 进程重启后有实际可用的原操作恢复通道，而不只是拒绝所有业务；热执行各阶段可以核验或明确恢复，遵守即时改密、不替换容器的语义。

### R5：旧版本 wake 锁与恢复接口的业务闭环

**缺口：** 新代码仅能据 `start_write_acknowledged` 收束特定 wake；现场旧版本记录没有该 checkpoint 时仍不能靠本轮修改自动清除。

入口：app_manager `lifecycle/wake.rs`、`lifecycle/recovery.rs`、`service/control.rs`；storage `common/ops.rs`；rcoder `userapp_builder/recovery.rs`。

实施要求：纳入 R2 的旧写核验，通过原操作恢复入口处理；不按租约超时/业务 Failed 自动释放，不手工改成功。区分已确认启动但等待失败与未确认写入。

完成条件：旧记录可以返回可执行的恢复步骤并据证据收束；之后新业务请求不再永久被历史锁阻挡。app129、app141 等现场状态会变化，本文不是当前现场快照，部署后必须重新读取。

### R6：接口与文档最后对齐

核查所有控制入口、自动唤醒、代理、后台扫描均遵守同一代次和 Stopped 语义。Java 文档与实际 202、operation_id、blocker、恢复阶段及 retry 对齐；Java 修改/联调不能算 RCoder 已完成。对 R1–R5 同步 Spec/Tasks，删除的是过期结论，不是原需求。

## 5. 编译状态及刚结束的补丁

- 最近日志 `/tmp/rcoder-archive-gc-scan-check.log` 已出现 `Finished dev profile`，对应 `CARGO_BUILD_JOBS=2 cargo check -p rcoder --all-features`。本交接只核对日志完成记录，未重新构建。
- 最近独立 app-cli 编译见 `/tmp/app-cli-migration-preflight-check.log`；PG 策略编译见 `/tmp/app-cli-pg-policy-check.log`、`/tmp/rcoder-pg-policy-check.log`。这些是历史具体批次证据，不是最终全工作树测试。
- 最新归档 GC 修改位于 shared trait `mark_restart_archive_cleaned`、storage `common/compute_execution.rs` 的扫描和完成 CAS、`recovery/compute.rs`。**修正历史 verification 中“忘记租约后也会扫描”的过早结论**：此前 SQL 未包含此类终态；最新补丁才加上。
- PG/Turso 使用不同 JSON 提取表达式筛选未清理归档。Rust 编译不能验证真实数据库 SQL，后续测试阶段需覆盖两后端。
- 本阶段未新增或执行测试；dirty tree 中已有测试文件来自此前批次，不能删掉或声称本轮已通过。
- `/tmp` 日志可能被清理；持久验证记录以同目录 `verification.md` 为主。接手后若日志不在，标明证据缺失。

## 6. 后续验证清单（待进入测试阶段执行）

先完成 R1–R6，再集中编译和有价值的验证，不反复跑整库：

1. 根默认/all-features、独立 app-cli 编译与 Clippy；按 AGENTS 用 nextest 验证受影响范围。并行 Cargo 不共享 target。
2. 真实 PG 独立连接竞争、Turso 同契约、v1→新增 schema 原数据升级；不能再次清库代替迁移。
3. 一个组合 E2E 覆盖：agent 自行启动 owner → 不同调用进程停止 → 重复停止 → 再启动/切换工作区；核验 owner、物理容器、卷和代理目标，而非仅端口。
4. 构建/发布中 Stop、Stop 接管 Restart、Stop 未完新 Restart 冲突、Stop 完成后启动；dev/prod 隔离。
5. 写响应丢失/回执前崩溃、控制器消失、RBD 原卷恢复、数据库缺登记、Stopped 不被被动查询唤醒。
6. 改密后已有连接/新连接、用户显式重启、owner 崩溃补交凭据、未知迁移不重跑；远程 PG/非默认端口及 standalone 默认不探测。
7. 阅读 Make 实际依赖后更新 Compose 二进制/镜像；app-cli 变动需更新业务 runtime 镜像，不用旧镜像证明通过。然后相应 test-e2e，最后个人 remote K8s 对应套件。
8. Java 全链及三平台实机另列结果。smoke、组件测试、编译均不替代部署验收。

不要操作故障现场来代替本地开发验证。app129 最终恢复需部署新接口后重新核验，通过恢复入口执行，保留 PVC 和失败历史；不能再次清库、直接清锁或手工标成功。

## 7. 跨仓边界

- `build-agent-docker`：镜像、RBAC、PG 管理身份和 K8s 配置与 RCoder 配套；接手先检查其 HEAD/diff。当前没有未提交文件不代表相关历史修复未提交或已发布。无需为了本文重建发布镜像。
- `userapp-workspace-template`：PG 等待策略说明已改为平台注入，不能再次给模板强制添加 `APP_CLI_REQUIRE_PG`。实际源码与提交状态重新读取；独立 app-cli 场景不深入 Electron。
- 不将测试机器账号、数据库密码、Bearer token 写入交接文档、代码或日志。

## 8. 可直接交给接手者的提示词

> 在 RCoder 当前工作树继续开发。先完整阅读 `specs/userapp-compute-control-recovery/remaining-development-handoff-2026-09-20.md`、其引用的 Spec/Plan/Tasks/verification 和 AGENTS.md，检查 git status/diff，保留所有已有未提交和未跟踪成果，不重置到 HEAD。按本文 R1–R6 补完生产逻辑，优先恢复链路与原操作凭据补交；先核对已实现分支，避免重复重写。遵守无内部排队、Stop 完成可启动、dev/prod 隔离、保留 PVC、即时改 PG 不重建容器、独立 app-cli 默认不检查 PG。当前先开发生产逻辑，可集中编译，暂不新增/运行测试，不提交、不推送、不发布、不改现场；测试阶段等待用户指示。持续更新完成范围和剩余项，不能将编译通过、暂时拒绝请求或局部恢复实现称为全部完成。

## 9. 交接后增量（2026-09-20）

- 归档扫描隔离单记录损坏；DELETE 后确认原 UID 消失才标记 GC 完成。
- 已停止 owner 不因旧脱敏密码新增启动 hold；Source 请求 journal 保留显式 PG 配置标记，密码仍不落盘。
- GET /v1/runtime/recovery 已提供只读恢复证据；R4 原操作凭据补交及业务续行依然未完成，不能把查询接口当恢复入口。具体编译范围见 verification 尾部。

## 10. 后续恢复实现增量（2026-09-20）

- 已终结旧执行且只有凭据保护时，显式 Source/Artifact 请求携带 PG 可按已确认制品、目录、迁移核验进入正常链；未知 kernel 保护不清除。
- 旧本地制品缺目录标记可在唯一制品匹配时恢复，URL 旧约定未直接套用。
- app-cli cancel 意图现在持久化并在 owner 重启后恢复。
- 启动确认旧进程已停止后，StartupFailed/Active 的未提交原操作可按证据收束 Failed/Cancelled，不伪造 Succeeded；原 Stop 与 Stopped revision 一致时可完成 Stop。
- 以上均仅编译验证。未知目录切换/迁移、原操作输入补交与续行、平台完整调用及 R1–R3 等仍未完成，不能据此把交接全部勾选。

## 11. 最新断点：平台部署入口尚未接通凭据恢复

本次源码直接确认：

- `crates/app_manager/src/lifecycle/hot_deploy.rs:323` 仍 POST `/v1/deploy`，164/442 附近查询 `/v1/deploy/status`；194 附近检查 `deployment_run_pg` 能力。
- `crates/app-cli/src/server.rs:891` 的 `try_accept_deploy_with_id` 仍在 owner recovery hold 时直接拒绝。
- 新的凭据保护核验在 `crates/app-cli/src/api/runtime.rs` 的 `submit_operation`，由 `server.rs` 的 `can_supply_run_credentials` 与 runtime dispatch 配合。

**因此不能声称“平台再次发布即可补交凭据并恢复”已经实现。** 当前只打通了部分独立 runtime 请求，不是生产平台调用闭环。

接手步骤：

1. 读 `prepare_hot_deployment`、实际 POST 请求及状态轮询、app-cli 两个受理入口和 dispatch，画清 operation ID、revision、运行配置的传递。
2. 选择统一 runtime 操作入口，或让旧入口复用相同受理核心；不能简单删掉旧接口的恢复保护。同步 operation 查询与错误传播，避免平台观察了另一条操作。
3. 区分“原未知操作按原身份恢复”和“旧操作已据证据终结后，新的显式操作补交凭据”。当前支持的是后者的一部分，不能拿新请求覆盖旧未知身份。
4. 核查仅凭据 hold 清除之后、实际准备失败的窗口：是否仍能保留正确的凭据缺失诊断，是否会回到制品默认密码。此项是待审风险，尚未证明已有缺陷。
5. 完整核对即时改密后的业务显式启动/重启与两个编排引擎，不为改密重建容器，不恢复默认密码，不重跑未知迁移。

## 12. 已补齐的目录切换恢复分支与验证边界

以下是第 10 节之后的增量，接手不要重复实现；也不要把它们外推为所有恢复场景已完成：

- 切换前记录目标制品 ID，启动确认旧进程静止后，按实际目录中的 release ID 分别处理“新制品已就位”和“旧制品仍在位”。journal 用完整快照比较更新，保留原失败身份，不伪造业务成功。
- workspace 缺失但 `.previous` 存在时，校验原 active、执行目录、目录类型与迁移回执，独占 preparation lease，再以不可覆盖目标的 rename 恢复旧目录。Linux/macOS 新增 rustix fs 依赖，Windows 使用目录 rename。
- 旧 HTTP(S) URL active 请求无 execution_target 时，凭据核验可使用 owner 启动时保存的不可变 workspace；本地制品不借此绕过 provenance。

对应入口：`deploy.rs::restore_previous_generation`、`server_journal.rs::confirm_switched_artifact/confirm_preserved_active`、`server.rs::initialize_startup/can_supply_run_credentials`。

最近独立 app-cli macOS `cargo check` 记录为退出 0：`/tmp/app-cli-previous-generation-check.log`、`/tmp/app-cli-url-credential-recovery-check.log`，详见 `verification.md` 尾部。本次仅核对记录，没有重跑编译。Linux/Windows 新分支未编译或实机验证；根 workspace 最近编译不能覆盖之后全部共享类型修改。未运行新增组件测试、Compose、remote K8s 或 Java 联调。

本文及本目录目前未跟踪，其他源码也有大量未提交修改。交接应直接使用现有工作树；若转移机器必须同时携带未跟踪文件，不能只传 `git diff`。本次不自动提交或推送。

## 13. 旧入口受理增量（接第 11 节）

`try_accept_deploy_with_id` 已复用同一凭据/制品/迁移核验，允许平台原 `/v1/deploy` 在纯凭据保护时受理新显式部署；未知 kernel/owner 保护仍拒绝。受理失败恢复凭据保护位。macOS app-cli check 退出 0，未测试。第 11 节“旧入口直接拒绝”是修改前断点，已补此受理分支；其第 3–5 项及受理后准备失败窗口仍未闭环，不能标记 R4 完成。

## 14. 制品缓存与运行配置增量

已修复 prepare 缓存命中时丢弃本次 PG 的路径；Running 热部署携带 PG 时会进入停止旧业务、重新编排流程，而非直接成功。Idle/Failed 同样传递 workspace/profile/PG。macOS app-cli 编译通过，未测试。准备失败后的恢复位生命周期及原操作续行仍未完成。

## 15. 凭据恢复失败保护增量

第 13–14 节所述临时放行生命周期已补生产实现：绑定 operation_id，受理失败、执行失败/取消时按 ID 恢复 credentials hold，确认 Running 后清除绑定；Stop 确认业务停止后收回 legacy 未完成放行。编译通过，未测试或部署。仍缺原未知操作的凭据补交/续行与完整 hot_execution 恢复，R4 不标完成。R1–R3、R5–R6 也仍按前文推进。

## 16. hot_execution 恢复输入增量

新增 receipt_protocol=1、物理 target、release_id 持久见证；捕获于部署提交前，执行前后核对。显式 PG 目标必须与热部署目标相同。旧 checkpoint 无见证不能自动补造。app_manager all-features 编译通过，未测试。下一步使用该证据完成原 hot_execution 的只读核验、配置收敛及 CAS 终结；不要把新增记录本身当作已完成恢复功能。

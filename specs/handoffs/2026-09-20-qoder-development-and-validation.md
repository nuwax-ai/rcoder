# Qoder 交接：剩余开发、验证与阶段提交

## 1. 结论与当前基线

**主实现已经大体落地，剩余工作主要是验证、失败修复和验收收口，但不能说“只剩跑测试”。** 最新 `retire` 和 guardian 命令摘要实现尚未编译；离线凭据交接只完成组件验证，真实 Docker/K8s 的默认字段、卷绑定、调度与恢复仍需检验。完整原生分发和跨平台验收也未结束。

- 仓库：`/Users/soddy/Documents/git-workspace/rcoder`，分支 `feature-userapp`。
- 本次交接前 HEAD：`bf2825ec`（已 push），其前为 `196d1776`、`3ce5694a`。
- 本文与当前工作树将一起作新的阶段提交；以包含本文的最新 Git 提交为本次交接基线。该提交不表示验收完成。
- 本次用户要求：整理交接并 commit 临时保存；**本次不 push、不构建发布镜像、不发布 npm**。后续发布单列为交付阶段。
- Codex 已停止新增实现，协作任务已冻结；集中测试已结束。接手仍应检查实际进程，不把旧日志尾部或锁文件当作活任务。

### 配套仓库

| 仓库 | 本次核对状态 | 处理边界 |
|---|---|---|
| `build-agent-docker` | HEAD `fb12fa6`；`k8s/helm/nuwax-platform/Chart.yaml`、`VERSION` 有用户未提交修改 | 保留用户版本修改。本次不代为提交这两个文件；配对构建时核对 PG 脚本、镜像与 Chart 版本 |
| `userapp-workspace-template` | HEAD `15ca1c1`，工作树干净 | 已有多语言连接串编码修改；作为真实模板验证输入，不无理由再改模板 |

不要将旧审计的“未实现”原文当作当前状态。例如 `specs/reviews/2026-09-19-batch10-implementation.md` 是历史记录，后来 Toasty/CR10 已继续实施。以源码和本文列出的后续证据更新完成矩阵，保留历史报告原基线。

## 2. 已有实现与证据，避免重复开发

| 范围 | 当前实现/证据 | 不能据此声称的内容 |
|---|---|---|
| SQLx → Toasty | 已切换共用 UserApp 存储、PG Project/Preview、正式 Turso 依赖、四份基线 SQL、CAS/身份/事务与关机保护 | T0 全部字段边界、最新全仓、个人集群换库验收仍未全收口 |
| PG 存储合约 | 独立 PG 最终 **31/31**，含正式 Preview 并发、跨身份约束、metadata CAS、重建后旧租约/请求隔离 | 不是远端双副本 K8s 业务通过 |
| CR01–CR09 | 已有实现及阶段组件记录，CR06 真实 Compose SIGTERM **15/15** | 最新版本完整 Compose、K8s 关机/多副本、完整 SSE 故障链仍需对应验收 |
| CR10 | 配置保存/生效分离、操作捕获版本、换代、source-seal/prepared 身份校验、原代次恢复、Stopped helper 已实现 | Java 尚未联调；真实最新部署链未通过 |
| app-cli | 较早完整全 features **272/272，1 skip**；其后离线 seal/legacy 入口 **2/2**，journal/离线追加 **15/15**；Clippy all-targets `-D warnings` 通过 | 272 轮不含之后全部离线入口和最新共享 guardian 改动，最终完整轮仍需补 |
| native owner/guardian | retire 之前 default、all-features 各 **486/486，0 skip**；all-features Clippy、proxy no-default Clippy 通过 | 不覆盖之后 `retire` 和 spec SHA-256 |
| 实际 native 进程 | macOS 较早二进制 **7 项**故障链通过 | 不覆盖后来 Pending 撤销小修、新 retire/spec；当前脚本新增目标为 **10 项**，尚未运行 |
| 本次最后集中测试 | `rcoder-storage + app_manager + docker_manager` 聚焦 **10/10**，退出 0 | 605 项为筛选排除，不是全组件通过 |

最后 10 项包含：codec 三项、helper 输入与回执两项、Docker/K8s spec 两项、既有 offline observer 两项、平台 source-seal 真调用链一项（内部含多分支）。平台 prepared 摘要/制品/revision 篡改用例此前另有通过，不要以 10 的总数替代逐断言核对。

## 3. 必须继续处理的事项（按顺序）

### P0：最新 native 改动先完成编译与真实故障验证

文件：

- `crates/process_utils/src/guardian.rs`、`Cargo.toml`
- `crates/file-server-proxy/src/{main,native_control,native_supervisor}.rs`
- `crates/file-server-proxy/npm/bin/file-server-proxy.js`
- `crates/file-server-userapp/src/handlers/userapp_dev.rs`
- `crates/file-server-proxy/tools/test_native_guardian.py`

最新待验证行为：

1. `retire --instance-id <原实例>` 核对原 token/实例，先持久 `Stopping + retirement_requested`，写出并 flush `RetirementAccepted`，退出原 owner（75）。客户端只有看到同一 supervisor 对同一 owner 的真实 wait 见证才返回 `OwnerExited`。不能把 Accepted 当退出完成。
2. 响应丢失按原实例观察重试；错误 token/实例不得让当前 owner 退出。退出不能伪写 Stopped、Quiescent 或外部 app-cli 操作成功；随后由显式 recover 基于原记录收束。
3. guardian 在 Pending 持久 spec SHA-256，Running/spawn 前重算；错 spec 未执行时可持锁 Revoked。已经 Running 的未知结果不得撤销冒充未执行。
4. Pending 与 stop 交错：确未 spawn 的拒绝应 Revoked；真实在途命令/TS 必须收树后才确认。
5. 新增 `sha2` 后检查根和 app-cli 独立 lockfile 的路径依赖清单，不顺带升级无关依赖。

先组件检查、再重建真实 proxy binary，然后执行当前脚本：

```bash
cargo check -p process_utils -p file-server -p file-server-userapp -p file-server-proxy --all-targets
cargo nextest run -p process_utils -p file-server -p file-server-userapp -p file-server-proxy --no-fail-fast
cargo nextest run -p process_utils -p file-server -p file-server-userapp -p file-server-proxy --all-features --no-fail-fast
cargo clippy -p process_utils -p file-server -p file-server-userapp -p file-server-proxy --all-features --all-targets -- -D warnings
cargo clippy -p file-server-proxy --no-default-features --all-targets -- -D warnings
cargo build -p file-server-proxy --bin file-server-proxy
python3 crates/file-server-proxy/tools/test_native_guardian.py --binary target/debug/file-server-proxy --report /tmp/rcoder-qoder-native-guardian.json
```

不并行运行共享 target 的 Cargo。已经通过且未再改动的阶段不无理由重跑；修正后按影响范围回归。脚本为 Unix 真实进程测试，不能在 Windows 上换个文件名就称通过。当前新增十项尚未实测，不要复用旧七项 JSON。

有意保留的未知保护：guardian 自身也死亡且无清理回执时仍拒绝恢复；心跳停止、端口空闲或 PID 不存在均不是解锁授权。旧无见证或缺 spec 摘要的持久记录不能简单删除，需核对正式升级语义与反例。

### P1：CR10 离线交接验证与必要修复

入口与规范：

- `crates/app-cli/src/{main,config,server,server_journal,runtime_kernel}.rs`
- `crates/app_manager/src/lifecycle/update.rs`
- `crates/app_manager/src/runtime/runtime_configuration.rs`
- `crates/container-runtime-api/src/{runtime_trait,types}.rs`
- `crates/docker_manager/src/runtime/{source_seal,docker_source_seal,k8s_source_seal}.rs`
- `specs/userapp-prod-wake-timeout-pg-credential/offline-source-seal-2026-09-20.md`

已实现：运行中 owner 先封存；物理停止时用独立 helper，不启动旧容器。helper 使用同 operation 的确定名称和持久 UID；Docker create/start 分开，K8s scheduling gate 在 UID 落盘后解除。原 seal 落盘后只继续原 helper 清理，不重复执行。结果未知保持原身份/租约。旧 journal 迁移包含 source-seal；`run`/`serve` 均遵守封存。

必须验证的真实边界：

- Docker daemon 默认 ENV/HostConfig 和 K8s API 默认字段不会误拒合法 helper，也不会容忍额外命令、init/ephemeral 容器、命名空间或不同卷。
- 原 workload UID、helper UID、PVC UID/spec 逐步重查；同名异物不可接管，未知 scheduling gate 不被顺手删除。清理只删确定 helper，不删卷/旧业务资源。
- K8s 使用原 Deployment 映像 spec，不应声称已核验旧 Pod 的 imageID。实际制品必须包含新 `seal-source` 命令；旧二进制缺命令应明确报错，不回退普通容器 start。
- create/start/日志读取/cleanup 丢响应后，恢复同一个 helper；source 已换代时不得再次替换；第三代拒绝。Stopped → 保存 → 显式无 URL Start 后账号、迁移、业务全部使用第二版本。
- 总 deadline 覆盖创建、调度、执行、清理；不能超时后删证据或直接放租约。
- 受控换代失败后显式恢复能够继续原操作，不通过换新 operation_id、修改默认密码或删 journal 解决。

组件已通过不等于业务已通过；真实回归入口已经存在：`tests-e2e/tests/compose_userapp_deploy.rs::verify_saved_runtime_credentials`，包括运行态切换和 Stopped 第二次切换。

### P2：完成数据库迁移验收矩阵与消费者核对

阅读 `specs/toasty-storage-unification/{tasks,schema-design,acceptance-matrix-2026-09-20,verification}.md`。

- 31/31 已覆盖后补 S01/S04/S09 和正式 Preview；旧矩阵正文保留早期“未运行”，看后续追加记录，不重复实现。
- 新 codec 三项通过只说明编解码：仍需真实 PG/Turso 合法极值写入读回、非法持久行/时间/JSON、NULL，以及完整 S01–S12 映射。能复用既有证据的直接引用，只补确实缺失的断言。
- 补完整依赖解析/Cargo tree、实际工具链/MSRV与TLS证据映射，不把 `metadata --no-deps` 当依赖兼容证明。
- 核对旧 SQLx、旧表/退役接口及配套脚本消费者是否全部收口；当前四份 schema 不是全消费者已核对的替代证据。
- 保留现有 PG/kubernetes feature 限制；不另扩 Compose PG 多副本需求。

### P3：重建当前二进制/镜像，再做 Compose

上次完整报告 `tests-e2e/reports/6b0904fb0cf745f095b0e39952fdc006/`：44 场景，34 pass、10 fail。

- 8 个 AI 场景缺少正确注入的私有 LLM 环境；补真实配置重跑，不能伪造响应或 skip。
- `two_users` 命中过去固定非法 app ID 遗留；测试隔离已经改，当前需要复跑证明，不能批量删除他人容器。
- `deploy_full_chain` 独立复跑报告 `dd511bc95c1a4f6cbaa5ab3d95d0571b` 暴露 CR10 源交接边界，之后才有本轮修复。该报告仍失败，不能被组件通过改写。

先核查这两份 summary、场景断言、报告基线和实际部署，再更新镜像。已存在完整 CR10 断言，不能把明确失败替换为长时间无条件等待。

命令语义（已核对当前 Make）：

- `make dev-hot` 只重编主 rcoder 并 restart，不会自动更新已有 builder/app-runtime 内的 app-cli/proxy。
- `make dev-restart` 依赖 `dev-build` 并重建 Compose 服务，不代表动态 UserApp 镜像/容器自动全部换新。
- app-cli/shared process/proxy 改动需要相应 `make docker-build-agent-runner` 和 `make docker-build-app-runtime`；核验两类容器真实二进制 SHA、Pingap 0.14.3、Python ABI/Java版本配对。
- 复用正确数据卷与私有配置；当前 Compose 有此前固定快照部署，不能只看 latest 标签或简单 up 后宣称用到了新代码。

先聚焦：

```bash
make test-e2e E2E_SUITE=compose_userapp_deploy E2E_FILTER=userapp_deploy_full_chain
# 依据真实 suite/test 名复跑 AI、two_users 和受影响恢复场景
make test-e2e
```

最后整轮必须有报告、非空覆盖、源码/镜像身份；筛选成功不冒充整轮通过。新增行为若套件未覆盖，补真正业务断言后再跑。长测试期间不替换部署或改动其源码快照。

### P4：个人 K8s/PG 新基线与整链测试

- 使用个人 `.131` 测试环境，从 `.env.local` 和 `.remote-k8s/` 读取实际 context/namespace/registry；凭据不写文档。
- 最近远端构建 `20260920T002721Z-798ba6d3` 镜像脚本/ABI核验通过，但早于最新 source-seal/native 修改，**不能作为当前源码部署证明**。
- 最近工作流环境 ID `c188d7e8de407557`；当时 namespace 为 `rcoder-e2e-soddy`。不要与 Helm 发布的 `nuwax-k8s-test` 混淆，接手必须重新核对。
- 已有旧对象/数据库预检记录，但个人目标 PG 的一次性重建和最新双副本部署尚未完成。用户仅授权个人测试库重建；先停止目标旧写入者，核对旧生命周期/物理资源归属，保留 agent PVC，不删除共享卷根或整个 namespace。
- 重建是基线切换，不是每轮失败都清库/清锁。其后验证持久化重启、两副本争用、断连、关机和 CR07 删除租约恢复。

按 `specs/remote-k8s-dev/README.md`：

```bash
make remote-k8s-doctor
make remote-k8s-verify SUITE=smoke
make remote-k8s-test SUITE=userapp
make remote-k8s-test SUITE=chat
make remote-k8s-test SUITE=gateway
```

追加套件只对同一已部署源码快照有效；变更源码后重新 verify。受影响 Preview、SSE、双副本场景若未覆盖，必须补，不以 smoke 代替业务验收。

### P5：两组件三平台与分发

按 `specs/native-desktop-runtime/tasks.md` 的 ND01–ND12、NT01–NT16逐项收口。旧 macOS/Linux/Windows 核心前端流程通过记录属于 `432fe129` 加当时工作树，不能覆盖当前 guardian/retire/proxy。

尚须重点完成：

- 最新 app-cli 和 file-server-proxy 三平台宿主机实际模板构建、重复启动、同 owner 更新、显式 restart/stop、未知3010占用、退出后子树清理。
- Linux/Windows 的新 guardian、retire、recover；Windows 正常 Ctrl+C 与 Job/活动文件升级。旧 Windows TerminateProcess 退出1不是优雅退出证明。
- NT13–NT16：只读/离线完整包、Pingap同版本配套、目标OS/arch/ABI拒绝、缓存隔离、运行中版本接管、显式TS兼容模式。直接exe与npm入口分别验证。
- 原生标准模式无需容器或外置 supervisor 服务；同binary guardian/supervisor属内部子进程。不深入开发Electron本体。
- 不默认关闭 public bind，不新增 user_id 绑定或 x-user-id 依赖。

### P6：Java 接入及发布交付单列

Java 同事接入说明：`specs/userapp-prod-wake-timeout-pg-credential/java-runtime-configuration-handoff.md`。保存成功仅表示待下一次显式启动/重启/发布生效；Java 联调尚未完成，不声称已上线。

Qoder先完成开发和测试报告。之后按用户发布安排处理版本、tag、npm和镜像，不用阶段commit冒充发布。镜像发布目标为配套仓库：

```bash
make setup k8s-helm-rcoder-version-publish ENV=test AMD64_ONLY=1
```

用户明确希望 Helm `--reset-values` 使用新 Chart 默认配置；不要擅自改成保存旧 values，也不要擅自覆盖 Chart/VERSION 的用户修改。使用实际发布版本，核验目标集群/namespace后再部署。npm、镜像构建、Helm部署、业务验收分别记录。

## 4. 证据与运行现场索引

主要详细记录：

- `specs/toasty-storage-unification/verification.md`（末尾为最新追加）
- `specs/toasty-storage-unification/acceptance-matrix-2026-09-20.md`
- `specs/native-desktop-runtime/nd06-07-control-completion-2026-09-20.md`
- `specs/native-desktop-runtime/{macos,linux,windows}-handoff-verification-2026-09-20.md`
- `specs/userapp-prod-wake-timeout-pg-credential/offline-source-seal-2026-09-20.md`

本机日志（接手先确认文件仍在，必要时脱敏归档，不直接提交凭据或整份 owner.json）：

| 证据 | 路径 |
|---|---|
| 最新10项聚焦通过 | `/tmp/rcoder-offline-storage-focused2.log` |
| app-cli272完整阶段 | `/tmp/rcoder-appcli-handoff-final-nextest.log` |
| app-cli15追加 | `/tmp/rcoder-appcli-offline-journal-nextest.log` |
| app-cli Clippy | `/tmp/rcoder-appcli-offline-clippy2.log` |
| native486默认/全features | `/tmp/rcoder-native-guardian-nextest2.log`、`/tmp/rcoder-native-guardian-allfeatures.log` |
| native旧binary7项 | `/tmp/rcoder-native-guardian-contract3.json` |
| PG31项 | `/tmp/rcoder-storage-new-pg-recheck.log`，报告根路径见 `/tmp/rcoder-storage-new-pg-recheck-report.txt` |
| 远端旧镜像核验 | `.remote-k8s/c188d7e8de407557/toasty-runtime-probe-20260920.json` |
| 原Compose部署选择/数据卷线索 | `/tmp/rcoder-handoff-compose-env.json`、`/tmp/rcoder-handoff-master-image.json`（只读核对，勿打印私有内容） |

交付前根 workspace 默认/全 features、app-cli 独立检查按 AGENTS.md 完成；尽量集中构建，修复后只重跑相关范围，再跑必要完整门禁。`cargo fmt --all -- --check` 与 app-cli独立fmt本次已通过；这不意味着最新retire通过编译。

## 5. 给 Qoder 的执行提示词

见用户消息中的提示词。以本文件为接续清单，源码为当前行为依据；不要缩减原需求、把待验能力直接改成不支持，或通过删除失败断言/清库清锁制造通过。逐项报告实现、组件、真实PG、Compose、K8s、三平台、发布状态，明确每项源码及二进制身份。

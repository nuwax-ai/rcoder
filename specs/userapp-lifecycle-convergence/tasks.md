# 实施任务

## 2026-09-14 用户要求移交 zcode

当前不宣布完成，不继续启动提交/镜像发布/集群部署。完整移交见 [HANDOFF-zcode-2026-09-14.md](HANDOFF-zcode-2026-09-14.md)。用户新增授权：本地 Compose 通过后 commit/push，构建仓 `make setup k8s-helm-rcoder-version-publish ENV=test`，再部署个人 `soddy@192.168.32.131` 的 `nuwax-k8s-test` 并做真实 K8s 回归；该授权替代前文“由用户部署”的旧边界。

串行验证队列已全部 exit 0，证据 `/tmp/rcoder-lifecycle-consolidated-gates/results.json`：fmt、三组 Clippy（-D warnings）、workspace（2094 pass/0 fail/35 ignored）、K8s runtime（180/0/5）、storage（109/0/1）、Python tools（90通过）。原 session 53655 已结束，没有待等待进程。普通 ignored/环境门控不能代替严格 E2E。尚未构建镜像、提交发布或操作集群。

## 当前执行状态：集中编译与验收

本节是当前执行入口；下方历史账本保留取证，不代表当前源码已通过旧门禁。

- 生命周期、双后端私有输入与身份绑定、builder 控制及 adoption、条件 lease receipt 释放与恢复扫描已落源码；三个 subagent 正并行复核是否仍有实现或覆盖缺口。
- 三份 Compose SQLite 接线、目录挂载和只读构建同步检查已完成源码修改；实际镜像、启动及重建持久化未验收。
- 严格 SQLite/PG/并发入口、独立报告和真实 SIGKILL 场景已落源码；Rust 场景尚未集中执行。旧 legacy marker 用例不能替代新 lease receipt 恢复证据。
- 当前 Kubernetes 特性 Clippy：`cargo clippy -p docker_manager -p rcoder --features kubernetes --all-targets --locked` exit 0，日志 `/tmp/rcoder-lifecycle-clippy-kubernetes-3.log`。两个测试任务收尾警告已修复，修复后待复跑。
- 默认 workspace Clippy 第五轮正在执行，日志 `/tmp/rcoder-lifecycle-clippy-default-5.log`；前四轮暴露的编译与 lint 问题已修正，不能标记默认门禁通过。
- 尚须：格式检查、完整默认/K8s/PG/SQLite 门禁及全量测试，独立真实 PG；本地镜像构建、dev-restart、dev-hot、运行产物身份核验、严格 E2E 与三 Compose 重建持久化验收。
- 新复核发现并正在补齐：Pending builder 显式 retry 分派；EnsureBuilder 就绪后的持久完成证据；新 lease receipt 终态扫描回归及严格 catalog 登记。
- 存储 `cargo clippy -p rcoder-storage --features pg,sqlite --all-targets --locked` exit 0，日志 `/tmp/rcoder-lifecycle-clippy-storage-final.log`；E2E Python 工具全套 82 tests passed，日志 `/tmp/rcoder-lifecycle-e2e-tools-final.log`。这不替代 Rust/数据库/容器验收。
- SQLite 测试第一轮 87 passed / 1 failed（测试矩阵复用了全局唯一 operation_id）；按应用区分测试身份后第二轮 88 passed / 0 failed / 0 ignored，日志 `/tmp/rcoder-lifecycle-test-storage-sqlite-2.log`。后续新完成证据回归仍待加入集中验证。
- E2E 工具加入冻结套件成员门禁后，完整 Python 工具测试 90 passed，日志 `/tmp/rcoder-lifecycle-e2e-tools-final-2.log`。
- Runtime 第一轮 `cargo test -p docker_manager --features kubernetes --lib --locked` 为 176 passed / 4 failed / 5 ignored，日志 `/tmp/rcoder-lifecycle-test-docker-kubernetes-final.log`。失败定位为 legacy 锁测试协议、403 结构化错误断言及 kube 空查询符；保留身份/条件写断言完成测试修正，第二轮日志 `/tmp/rcoder-lifecycle-test-docker-kubernetes-final-2.log` 待终态。
- 收尾源码已收齐。`cargo fmt --all --check` exit 0（`/tmp/rcoder-lifecycle-fmt-consolidated.log`）；Runtime 第三轮 180 passed / 0 failed / 5 ignored（`/tmp/rcoder-lifecycle-test-docker-kubernetes-final-3.log`）。既有 ignored 不能计入严格验收通过。默认 workspace Clippy 第六轮运行中，日志 `/tmp/rcoder-lifecycle-clippy-default-6.log`；新 builder 完成证据、retry 与scanner组件仍待Rust测试。
- 三份 Compose 当前配置解析均 exit 0；未启动或重建现有业务容器。默认 Clippy 第七轮发现 Pending builder 恢复返回值不一致，统一 `bool` 以保留未抢到执行权的冲突语义，不能 map 丢弃结果。存储双特性测试运行日志 `/tmp/rcoder-lifecycle-test-storage-combined.log`；未提供 PG DSN 的普通测试不能充当真实 PG 验收。
- 独立真实 PG17 契约完成：`/tmp/lifecycle-pg-409f10bf7a274a7591aea20d934494a7/`，执行 exit 0，7 个原项目时序用例及 userApp 真实事务/重启用例均通过；assertions.json 10 项全 true（含环境及清理），cleanup.json ok=true。本次 Compose 项目 `rcoder-pg-4af5bb08be124b7f` 已定向清理。
- 存储双特性普通测试 109 passed / 0 failed / 1 ignored（真实 PG 显式用例另按上一条执行），日志 `/tmp/rcoder-lifecycle-test-storage-combined.log`。默认 Clippy 第八轮正在执行 `/tmp/rcoder-lifecycle-clippy-default-8.log`。
- 默认 `cargo clippy --workspace --all-targets --locked` 第九轮 exit 0，日志 `/tmp/rcoder-lifecycle-clippy-default-9.log`；2 个测试警告随后修正（引用构造slice及显式校验retry响应operation_id），待最终无警告复核。`cargo test --workspace --locked` 正在运行，日志 `/tmp/rcoder-lifecycle-test-workspace-final.log`。
- workspace 首轮在 app_manager 163 passed / 10 failed；随后排除此模块执行其余 workspace（--no-fail-fast），仅 rcoder 266 passed / 5 OpenAPI failed、file-server-userapp 72 passed / 3 OpenAPI failed，其余执行目标通过或按普通环境门控。日志 `/tmp/rcoder-lifecycle-test-workspace-final.log` 与 `/tmp/rcoder-lifecycle-test-workspace-rest.log`。
- 实际修复：流量唤醒不能在Ready之前mark_running；更新final checkpoint须保留target/storage_target证据。其余校准旧测试前置/错误类型、补新HTTP文档及固定路由清单。三受影响crate统一复跑 `/tmp/rcoder-lifecycle-test-affected-2.log`，尚未当作通过。
- 受影响模块复跑：app_manager 174 passed、rcoder 271 passed；file-server-userapp 74 passed / 1 failed（连续prepare间的文件锁释放窗口）。新增复制文件描述符确定性回归得到红灯 `/tmp/rcoder-lifecycle-staging-lease-red.log`，修复显式unlock与Drop兜底，保留staging先清理后解锁顺序；全crate绿灯验证运行 `/tmp/rcoder-lifecycle-staging-lease-green.log`。新回归已纳入strict并发目录。
- 准备锁显式释放修复后 file-server-userapp 全套 76 passed / 0 failed / 0 ignored，日志 `/tmp/rcoder-lifecycle-staging-lease-green.log`。
- 已启动串行统一门禁，证据目录 `/tmp/rcoder-lifecycle-consolidated-gates/`，`results.json` 每项终态写入；顺序 fmt、默认/K8s/PG+SQLite Clippy（-D warnings）、workspace测试、K8s测试、存储测试、Python工具。任何非零立即停止，尚未执行的项目不能计为通过。完成后仍须构建和实际Docker严格验收。
- 未访问集群、未提交或发布；完整计划仍未完成。Cargo 使用独立 target 且串行执行，避免并发抢占编译资源。

## 已完成调研

- 核对实际 K8s 失败报告、shared_types 元数据契约、PG upsert/delete、metadata cache、两族操作锁和 handler 调用顺序。
- 查阅 Kubernetes/client-go、kube-rs、PostgreSQL17、AWS 幂等与 outbox 官方资料。
- 明确：缓存不可裁决删除；lease 过期不是 fencing；纯 DB 事务不能包住 K8s 副作用。

## 待执行批次

1. 固定 stop/prod delete/purge/dev destroy/整应用删除范围矩阵；列全所有 userApp 写入口、锁顺序和元数据登记点。
2. K01：409→重读同 UID→重试成功；UID 变化/403/响应丢失不误重试。K03：稳定错误分类贯通各正式入口。真实 K8s 首次并发 ensure复验。
3. 定义 shared_types 身份/版本/命令/结果；实现 PG 权威命令和 SQLite 持久化后端，同一契约覆盖幂等 owner、patch不覆盖其他字段、旧生命周期补写拒绝、重启恢复。
4. 移除旧 metadata 无条件 upsert/缓存裁决；删除 handler 不再登记 owner；为 A/B 副本旧缓存场景补真实 PG 测试。
5. 应用级协调器一次受理，下层传 operation context；覆盖当前 builder/prod 写入口，证明无重复拿锁和读锁升级。
6. 持久化 purge步骤与恢复任务；删除墓碑、重复请求、并发登记、取消、进程重启、PG故障、远端结果不确定分别有回归证据。
7. 检查每类 runtime 写面的身份条件；未满足自动接管前置条件的步骤明确进入 RecoveryRequired，测试不允许静默超时接管。
8. 默认/K8s/PG 编译和 Clippy、workspace 测试、真实 PG 契约、SQLite 重启恢复；变更 app-cli 才单独进行其工作区及发布相关验证。
9. 构建本地镜像，Compose回归；平台镜像由用户升级个人 K8s 后执行固定 userApp E2E，增加“受理副本退出，另一副本恢复”的独占测试，禁止影响既有应用。
10. 最终验收要求新契约无失败/跳过/中止/报告遗漏，明确区分组件、真实 PG、Docker、K8s 和人工恢复证据；定向清理完成。

前一轮 34 通过/2 失败属于旧实现集群基线，不能当作本方案通过证据。本次代码与验证进展见下方实施账本。

## 本次实施账本

- 基线主仓 61998742；已有 E2E 工具/报告文档保留。构建仓 Chart.yaml/VERSION 用户修改保留。
- 本地 SQLx Cargo.toml version=0.9.0；仅作实现参考。
- 已更新已批准 SQLite/首开/三 Compose 行为边界，旧调研文件后端方案作废。
- [x] B1 PVC 冲突重试：先增加 HTTP 契约回归并记录红灯，再修改实现。
- [ ] B2 shared_types 操作身份、双后端存储、迁移与故障契约。
- [ ] B3 协调器及全部入口接入、首开收敛、删除与恢复。
- [ ] B4 HTTP 收尾及三 Compose/构建配置。
- [ ] B5 编译、Clippy、全量测试、镜像、Compose 严格验收。

以下仅登记实际命令证据，不以旧集群基线或组件通过替代新版本端到端验收。

### B1 / B2 当前证据

- `cargo test -p docker_manager --features kubernetes --lib storage_claim_`：旧实现 2 passed / 2 failed（实际业务断言红灯，日志 `/tmp/rcoder-lifecycle-b1-red.log`）。
- 相同命令修改后 4 passed / 0 failed，日志 `/tmp/rcoder-lifecycle-b1-green.log`。覆盖同 UID 重读成功、UID 替换、403、四次预算。尚未宣称全部 K8s 回归通过。
- `cargo check -p rcoder-storage --features pg,sqlite` exit 0，日志 `/tmp/rcoder-lifecycle-storage-check.log`。新增存储尚未装配到 AppService，不是端到端证据。
- SQLite 契约初期曾等待 IDE workspace Cargo lock；结果见下节，用户/IDE 的其他构建未被终止。
- B2 的 request_id 别名与 PG 实测已在下节完成；旧表回填、列表投影/扫描分页、HTTP/runtime 接入仍未完成。当前不得启用新后端配置或标记总计划完成。

### 第一批验证结果（完整计划尚未完成）

- 已实现 PVC 同 UID 条件重试，以及现有元数据删除的旧缓存假冲突修复；保留后来到达的新缓存写入。
- 新增 shared_types 生命周期/操作契约、SQLite 与 PG 存储基础、独立迁移记录、请求去重别名、执行者身份校验、元数据 CAS 和显式重建。**尚未注入 AppService 或任何业务 HTTP/runtime 入口**。
- SQLite/PG 严格组件命令：`cargo test -p rcoder-storage --features pg,sqlite --lib userapp_lifecycle -- --include-ignored`，13 passed / 0 failed / 0 ignored。PG 使用本次独立 PostgreSQL 17 实例，DSN 未指向日常服务。等待者去重回归在 SQLite 与真实 PG 都先得到红灯。
- `cargo test -p app_manager --lib`：132 passed / 0 failed / 0 ignored。新增旧缓存、权威不存在和迟到通知/新本地写三项回归；旧缓存用例修改前明确失败。
- `cargo test -p docker_manager --features kubernetes --lib`：155 passed / 0 failed / 5 ignored（既有 ignored，不作为严格 E2E 通过）。
- `cargo fmt --all --check`、storage(pg+sqlite)/app_manager/docker_manager(kubernetes) all-targets Clippy `-D warnings`、SQLite-only 编译均 exit 0。
- 独立 Cargo 目录 `/tmp/rcoder-userapp-lifecycle-target`，并行度 2；默认 target 的 IDE 检查未被终止。迁移克隆的 CMake 缓存路径失败后，仅清理本次独立目录中的旧 CMake 构建缓存并重建，未修改依赖源码。
- 证据目录：`tests-e2e/reports/lifecycle-components-4442bacd29cb4d4c9c2dc2cd805b4d94/`。summary.json 明确标记 component-only / plan_complete=false，并保存源码哈希、命令结果、PG 物理身份与清理记录。
- 临时 PG schema 已全部删除（清理前计数 0）；本次容器及匿名卷已定向清理。既有用户应用与数据未操作。

### 必须继续，不能据此发布

1. 完善存储批量投影、恢复扫描分页、安全重试受理与旧 PG 元数据回填；当前扫描接口尚未供运行任务消费。
2. 接入统一协调器与结构化 OperationInProgress：公共 builder ensure 跨副本等待、全部生命周期入口一次受理、取消与删除顺序、持久化步骤及恢复执行器。当前真实首开接口仍未接入新等待机制。
3. 替换旧元数据无条件 upsert/全量缓存权威路径；删除 handler 去掉登记 owner；归属读取错误必须向上传播。当前缓存修复仅解决删除假冲突，不等同于整体替换。
4. 实现操作查询/安全重试/显式重建 HTTP、OpenAPI/i18n、流式错误收尾。
5. 同步三份 Compose、后端配置与编译特性、宿主机 SQLite 目录、启动/备份/健康检查及隔离重建验证。构建仓 Chart.yaml/VERSION 原修改保持未动。
6. 完成 workspace 默认/K8s/PG/SQLite 全量门禁，并在全部入口接入后构建镜像、dev-restart 与完整验收；当前批次的 dev-hot 和两轮严格本地 E2E 已执行，见下节。镜像重建、SQLite 服务持久化及新协调器验收仍未执行。K8s 运行验收待用户升级镜像。
7. 未修改 app-cli，无 npm/tag/镜像发布，无 git commit。

### 2026-09-13 当前批次本地 Compose 验收

- 用户要求先在本地编译启动并测试。仓库没有 `dev-host` 目标，实际使用 `make dev-hot`；容器内 release 编译 1m50s，命令 exit 0，随后成功重启 rcoder。
- `cargo test -p rcoder-e2e --locked --no-run` exit 0；宿主机使用独立 `CARGO_TARGET_DIR=/tmp/rcoder-userapp-lifecycle-target`、`CARGO_BUILD_JOBS=2`，未占用 IDE 的默认 target。
- 显式 `RCODER_URL=http://127.0.0.1:8090 make test-e2e`：exit 0，34/34 场景通过，438 项 hard assertions、0 失败，无跳过/中止/报告遗漏。报告：`tests-e2e/reports/da5c5a6982fb4d1b8c2170c3db79844b/summary.json`。
- 同样配置执行 `make test-e2e-compose`：exit 0，55/55 场景通过，333 项 hard assertions、0 失败，无跳过/中止/报告遗漏。报告：`tests-e2e/reports/d6ad216717aa43cc94f621a59b36efac/summary.json`。两入口存在重叠场景，不能称为 89 个独立行为。
- 覆盖真实 AI 双后端、主平台 SSE/会话/WebChat、开发任务与构建规则、7 服务全量构建/制品摘要/部署、失败更新后真实代理响应、容器内热部署两种引擎、静态目录与端口切换、stop 后唤醒、purge、Docker 删除身份及独立 PostgreSQL 17 既有项目存储时序契约。
- 两轮报告的源码指纹前后一致。验收结束后仅追加本节文档，未修改 Rust、测试或运行配置。
- 最终 `/health` 的 HTTP/gRPC 均 ready；`/proc/1/exe`、`/app/bin/rcoder`、`target-console/release/rcoder` SHA-256 均为 `50335d079ddc583fbcaa51b6b234781633e8b76236c3fe9505b13e2d190ed8b7`，证明运行的是本次编译产物。
- 两轮 run 所属容器最终库存均为 0，逐场景清理校验通过；独立 PG Compose 项目已删除。健康及哈希、清理复核见各报告目录 `local-final-verification.json` 和运行证据文件。
- 边界：本次更新主服务 binary，未重建 builder/runtime 镜像；SQLite 新存储仍仅有组件证据，尚未配置到运行服务。首开跨副本收敛与恢复协调器未接入，不能用现有 E2E 全绿宣称整个生命周期计划完成。未访问 K8s 集群、未提交或发布。

### 提交前复核

- 用户明确要求提交并 push，以准备个人测试集群镜像构建；提交范围为当前 rcoder 修复、存储基础、K8s 验收工具及 Spec/Plan/Task，不包含构建仓的用户修改。
- `cargo check -p rcoder --features kubernetes` exit 0，1m45s，日志 `/tmp/rcoder-prepush-k8s-check.log`。这证明当前主程序 Kubernetes 特性可编译，不代表真实集群运行验收。
- `python3 -m unittest discover -s tests-e2e/tools -p test_k8s_userapp.py`：4 passed；格式与 staged diff 检查通过。

### 完整计划继续实施（42541f3 之后，未完成）

- 目标保持完整：运行服务接入、首开并发收敛、协调器/删除恢复、三份 Compose 和新增验收均不能用既有回归全绿代替。
- 存储增加 app_id 游标分页、operation_id 游标分页与旧元数据只插入导入；扫描中首条 RecoveryRequired 不再遮蔽后续页。导入拒绝缺失 owner/归属冲突，重复导入保留当前生命周期与墓碑。
- 修正 DestroyProdStorage 被误归类为结束生命周期；先得到 `Deleting != Active` 的行为红灯，再修正仅 DeleteApplication 结束生命周期。红灯日志 `/tmp/rcoder-lifecycle-storage-scope-red.log`。
- SQLite 新增部署专用独占打开方式：每数据目录文件锁、NFS/SMB 拒绝、阻塞线程执行文件系统检查。重开身份保留与第二实例拒绝已通过组件测试。
- Agent 与 userApp 共用原 PG 连接策略函数，但 userApp 使用独立 pool，不启用 Agent write-behind；增加现有 userapp_metadata 导入入口。
- `cargo test -p rcoder-storage --features sqlite,pg --lib userapp_lifecycle -- --include-ignored`：16 passed / 0 failed / 0 ignored。真实 PostgreSQL 17 实例与 schema 已清理。证据：`tests-e2e/reports/lifecycle-storage-fae3833adcf7443ab210e7e98069ec05/`。
- 第一次 PG 验证因初始化临时 Unix socket 提前 ready，SQLx 外部连接失败；第二次仅收紧为 TCP 就绪检查后通过，未重试业务断言。相同就绪修正同步到 `tests-e2e/tools/pg_contract.py`。
- 新增独立 userapp_storage 配置与初始化工厂，默认 Docker→SQLite、K8s→PG；编译特性配套。**该次验证时工厂尚未接入 AppState/AppService；后续接入进展见下方，运行容器尚未切换**。`cargo test -p rcoder --lib config::userapp_storage` exit 0，3 passed / 0 ignored，覆盖指定文件持久化/重开、独占拒绝、非法目录和配置 fail fast；日志 `/tmp/rcoder-userapp-config-tests.log`。`cargo check -p rcoder --features kubernetes` 在独占打开补充前已 exit 0；最终新代码仍需重跑完整特性门禁。
- 下一接入链：`runtime/metadata.rs` 去缓存权威并传播 Result → `AppServiceTrait::get_app_owner` 不吞读取错误 → AppState 注入新 store → 公共 builder ensure 与 prod/deletion 操作统一受理。恢复执行器和新 API 必须接着完成，不能在打开数据库后停止。

### B2/B3/B4 业务接入批次（仍未完成整个计划）

- AppState 已调用独立 userApp storage 工厂并强制注入 AppService；移除依赖 Agent PG 开关的旧 metadata cache 装配。`get_app_owner` 改为 Result，数据库故障向 builder/cache 清理调用方上抛。
- AppMetadataStore 不再持有内存权威副本。列表按游标批量读取；字段 None 保留，重复相同身份/内容不改版本。访问 URL 从已读取的 owner 构造，不在排序/同步闭包中访问数据库。
- 新增实际红灯：已有其他 owner 时，创建请求在返回归属冲突前调用了 runtime create 1 次，预期 0 次。证据 `/tmp/rcoder-owner-admission-red.log`。随后前移归属校验与元数据提交；创建、更新不再先改资源后发现归属错误。此测试不证明并发删除 fencing 已完成，统一操作受理仍待完成。
- 完整 purge 先登记/认领 DeleteApplication 操作，资源快照写入 checkpoint 后再清理；成功原子提交 Deleted 墓碑后才释放旧资源锁。中途错误保留失败或 RecoveryRequired；单独 storage/destroy 保留应用身份。步骤恢复、未知身份的权威查空幂等、全部写入口统一协调尚待完成，不能称为恢复链已验收。
- 应用管理测试使用实际临时 SQLite，保留 runtime 基础设施测试适配器。修正旧测试 fixture 中创建 user_id=u-test、更新 user_id=u1 的不一致；不放宽归属校验。新增跨连接读取、同内容版本稳定、关闭连接后错误传播及重开保留测试。
- `cargo test -p app_manager --lib` 当前最终结果 exit 0：128 passed / 0 failed / 0 ignored。日志 `/tmp/rcoder-metadata-tests.log`。第一轮迁移 fixture 时 123/124，缺 owner 的卷路由 fixture 已补实际登记；第二轮前移校验时 120/125，上述五个 owner 不一致 fixture 已修正。
- `cargo check -p rcoder` 接入初稿 exit 0，日志 `/tmp/rcoder-metadata-check.log`；后续测试/归属校验变更的最终完整门禁尚待核验。
- 三份 Compose 已显式选择 SQLite，并挂载 `${RCODER_DATA_DIR:-./data/rcoder}:/app/data`；新增各自的 named-volume 覆盖和 `.env.sqlite.example`/`SQLITE.md`。三份各两种配置共六次 `docker compose config --format json` 解析与字段断言通过；启动脚本 `bash -n` 通过。构建脚本均保留 default features，因此包含 userapp-sqlite；实际镜像/二进制仍需构建核验。
- 本批没有重建运行容器、没有运行新 E2E，没有清理既有应用/数据库。当前主服务不能用本批源码测试结果冒充运行中 SQLite 验收。

- 当前批次 `cargo clippy -p rcoder -p app_manager --all-targets --features kubernetes -- -D warnings` exit 0，44.25s，日志 `/tmp/rcoder-metadata-clippy.log`。覆盖主程序、测试装配和 K8s/PG 编译路径；不是全 workspace Clippy，也不是实际集群验证。

- 删除 HTTP 入口回归先实测失败：没有持久身份时旧 handler 先登记请求 owner 并成功删除资源；日志 `/tmp/rcoder-delete-owner-red.log`。已改为只读现有 owner 校验，查询失败/不存在/归属冲突均阻断删除，不再 best-effort 登记。新增测试验证无登记、无删除别人的资源，已计入 128 项通过结果。
- 接入后 `cargo test -p rcoder --lib config::userapp_storage` exit 0：3 passed / 0 ignored，日志 `/tmp/rcoder-live-storage-config-tests.log`。仍是实际 SQLite 组件层，非容器重建验证。
- 后续明确待办：完整操作上下文和客户端 lifecycle_id 接入；同 owner 并发写/删 fencing；builder 共享创建和跨副本等待；启动导入现存 runtime 身份；安全步骤重试/恢复及其阶段分类；未知身份的权威查空幂等；storage query 当前吞单项错误的旧路径；正式操作 API/i18n/HTTP 收尾；全部构建和严格 E2E。

- `cargo test -p rcoder --lib container_status_checker` exit 0：6 passed / 0 ignored，日志 `/tmp/rcoder-storage-status-checker-tests.log`，验证新增真实 SQLite 依赖注入后的现有状态检查器测试装配。

- 最终 handler 改动后的同一 Clippy 命令再次 exit 0（13.66s）。本批组件/编译与修复前红灯日志归档：`tests-e2e/reports/lifecycle-wiring-c5d28af6acbb4b329f0d4efe74d5b901/`。该报告明确不是 Docker/K8s 运行验收，也不代表计划完成。

### 首开收敛实现批次（按用户要求集中开发，暂未编译/运行测试）

- 用户调整执行节奏：剩余实现和用例集中补齐后再统一 Cargo 编译/Clippy/测试；不再逐个小改动触发编译。此前 128/3/6 与 Clippy 只证明上一批，不覆盖以下新代码。
- 新增 shared_types::UserAppOperationInProgress，明确区别资源 CAS 与操作锁占用；K8s 409 后只读查询赢家 operation_id，缺失标记视为旧/未知锁，不把输家 UUID 当赢家身份。
- builder 创建增加持久化 EnsureBuilder 受理、执行者认领、独立工作任务和跨副本状态轮询。调用方取消不丢执行任务和本地 lease；通知使用 watch::send_replace，最终判断仍读取存储。默认 200ms→2s 加抖动，带同一等待截止时间；创建后核验资源身份及 file-server 就绪再提交成功。
- 已增加等待超时不取消/重提操作、晚订阅读取已提交结果、owner 不可覆盖、官方定位键优先级与跨族拒绝的源代码用例；尚未运行。
- 注册表 miss 增加只读 runtime 发现；探活/交叉校验的 runtime 失败不再伪装 Gone，调用方传播错误；运行资源核对类型、官方定位键和 owner，查询成功但容器不存在不复用旧地址。
- 当前仍需完善：完整 runtime 操作上下文/代次标签与配置指纹的实际配置收敛；首开受理前整个调用链的统一截止时间；持久化错误的稳定 HTTP/i18n 映射；恢复扫描和 Pending 操作派发；全部 prod 写入口统一受理；生命周期请求字段与新 API。新增代码仍处于集中开发阶段，不宣称首开 E2E 通过。

### 操作接口、Pending 恢复与 HTTP 收尾（集中开发，未编译/执行测试）

- 增加生命周期查询、当前操作查询、按 ID 查询和显式 recreate 路由；接入主 OpenAPI。跨 crate 请求/操作投影定义在 shared_types；查询校验 owner，公开投影不包含 executor/checkpoint/fingerprint。recreate 校验旧生命周期及 request_id，检查残留 prod/builder；重复请求返回相同新身份，不因后来新容器存在而拒绝原请求的结果查询。
- 已编写真实 SQLite 上的操作查询归属/内部字段隐藏、重建代次/重复请求/旧操作隔离用例；未执行。安全 retry 接口、所有修改请求的 lifecycle_id 透传尚未完成；recreate 路由目前是待整体验收的开发代码，不能单独发布。
- Pending builder 恢复复用同一执行函数：启动及每 5 秒分页扫描，只恢复未认领且配置指纹/生命周期匹配的 EnsureBuilder。SQL 版本 CAS 决定唯一执行者；进程内 try_acquire 避免扫描被活跃应用阻塞。配置变化转 RecoveryRequired，不清旧资源锁；Running/未知操作不因超时而接管。已编写本地 lease 非阻塞回归，未执行。
- 新增错误请求体收尾模块，接入 dev/prod 转发前定位失败和只读查询拒绝路径；只影响拒绝收尾，默认 1 MiB/1 秒，正常上传/下载/解压仍无限额。已编写 complete/body-error/limit/stall 用例，以及慢速 multipart 使用真实 TCP+axum 接收原错误 JSON 的用例，未执行。其他提前返回路径覆盖仍待逐入口审计。
- storage 查询改为权威 metadata owner/tenant/space 过滤，runtime/stat/locator 查询失败向上传播，不再把单项错误标成不存在。原存储路由测试补齐实际 owner/locator fixture；尚未运行。
- 继续待办：全部 prod 命令持久化受理和参数保存；完整 runtime 操作上下文/生命周期标签/配置身份；精确 checkpoint 恢复与安全重试 API；统一截止时间和稳定错误码/i18n；修改请求代次约束；集成/E2E 契约登记；统一编译与实际构建验收。

### 运行时操作身份传递（集中开发，未编译/执行测试）

- shared_types 增加 UserAppExecutionContext，由已认领的 builder 操作构造，经 ContainerCreateParams 传入 runtime；包含 owner、lifecycle、operation、executor 和配置指纹。K8s/Docker 创建入口在取得操作锁前验证上下文与正式定位键、owner 一致。
- K8s ConfigMap 锁与 PVC claim 使用持久化 operation_id；PVC claim 在原 UID/resourceVersion 条件写入上记录 lifecycle/owner，拒绝已标记的不同生命周期或归属。原 409 重读、4 次/3 秒预算保持。旧无上下文锁单独记录 legacy-operation-id，不能被推断为新的持久化在途操作。
- Docker 文件 marker 使用同一持久化 operation_id；即使重试提供相同 ID，非空 marker 仍要求明确恢复，不允许身份相同就自动接管。
- 已编写执行上下文归属/凭据校验与 Docker marker 回显、完成清理、同 ID 禁止接管回归；既有真实 HTTP K8s PVC 冲突测试增加实际 PATCH 中固定操作/生命周期/owner 的断言。均尚未运行。
- 计算资源自身的生命周期/配置身份、所有 prod 操作上下文、显式恢复授权仍未完成；本批仅接通 builder 执行凭据和存储 claim，不作为完整恢复契约通过的证明。

### Builder 计算资源身份和扩缩容（集中开发，未编译/执行测试）

- 共享执行上下文生成 runtime 元数据；Docker builder 配置写入 labels，K8s builder StatefulSet 与 Pod template 写入 annotations。运行时创建入口在副作用前校验上下文服务族和官方定位键。
- K8s builder 专用 ensure 不再经过 Agent 类型/挂载漂移的删旧重建逻辑；创建冲突读取赢家并校验 owner/lifecycle/config/template/实际启动字段/PVC。带上下文时不走仅依赖本地 pod cache 的早返回。
- StatefulSet scale 改为捕获对象后使用 UID/resourceVersion 条件 PATCH；同 replicas 无写入，类型不符、删除中或缺物理身份拒绝。不自动重试 CAS 冲突去修改替代对象。
- 新增捕获 UID/version PATCH、错误服务族/缺版本、生命周期/模板身份和不同操作来源复用的组件用例源码。格式整理已执行，Cargo 门禁仍留到开发完成后统一运行。
- 尚缺实际 K8s HTTP 请求级 scale/创建冲突回归、遗留资源显式接纳、Docker 复用路径对应校验和全生命周期控制协议接入；不能据此宣称整个身份/恢复链路完成。

### Docker builder 复用与健康失败保留（集中开发，未编译/执行测试）

- 带上下文的 builder 跳过 AgentContainerStarter 的旧容器清理；ContainerCreator 复用前直接 inspect，核对服务族、定位键、共享生命周期/owner/config 身份和实际 image，再按捕获的物理 ID 查状态和启动。
- 停止容器只有明确 created/stopped/exited 才可按 ID 启动，不走 remove 后重建；未知、删除中、暂停等状态返回明确错误。POST 创建 409 对赢家做相同身份校验，禁止无条件删除赢家。
- ContainerCreator 与 AgentContainerStarter 两层健康检查失败都保留带上下文的 builder，不再探活失败即 stop；不带 userApp 上下文的 Agent 现有行为保持。
- 新增 builder 复用身份矩阵源码，覆盖不同原创建 operation 可复用、不同 owner/lifecycle/config/image 和缺物理 ID 均拒绝；旧缓存测试调用适配新内部参数。尚未执行 Cargo 或实际 Docker 回归。
- 仍需补充实际 HTTP Docker 故障注入/并发赢家验收、完整 effective-config 校验和无标签资源接纳；此批并不构成整个计划完成。

### 请求去重跨重建边界（集中开发，未编译/执行测试）

- 核实普通操作与合并 ensure 已在同一事务登记 request alias；不重复替换现有机制。补上显式 recreate 表与普通 operation request 表之间的双向冲突检查，同一应用的 request_id 不允许从普通操作复用为重建，或从重建复用为新生命周期的其他命令。
- 操作/执行者 ID 在存储受理和认领前验证，避免到 runtime 生成身份凭据时才失败；request_id 的非空及 128 字节边界在两个 SQL 后端和重建入口统一。
- 新增共用契约并接入 SQLite 和实际 PG 套件：跨命令 request_id 复用冲突、精确重建重复查询仍幂等、非法请求受理不留下新 identity。新增用例均未执行，仍按用户要求最后统一运行。

### 元数据与操作原子受理（集中开发，未编译/执行测试）

- UserAppAdmission 增加可选 metadata patch；已受理操作保存原补丁用于精确重试判定。存储在当前同一短事务内执行 metadata revision/owner/lifecycle 校验、字段变更与操作登记，受理冲突不先改元数据。
- 提取共享纯 metadata transition，独立字段补丁与原子受理复用；保存补丁时保留“未传字段”和“显式 null 清空”的区别，避免 JSON 往返后重试语义变化。当前业务调用默认不携带 patch，生产入口仍需继续接入，不能将底层能力当作端到端完成。
- SQLite/PG 共用契约增加：旧 metadata revision 拒绝且无 operation 残留、成功受理同时修改版本和 current operation、相同 request 精确重试不重复写、参数改变/竞争操作不改变配置；包含显式清空字段的序列化回放。
- SQLite 增加真实 SQL trigger 故障注入：operation INSERT 后 lifecycle UPDATE 失败，验证整个事务回滚，解除故障后相同请求可受理。尚未执行任何新增测试。

### start/restart/update 请求生命周期（集中开发，未编译/执行测试）

- StartAppRequest、UpdateAppRequest 增加可选 lifecycle_id 并随现有 ToSchema 纳入 OpenAPI；首代可缺省，显式重建后必须传当前生命周期。校验读取权威存储，拒绝 foreign owner、Deleted/Deleting、旧代 token 和二代以上缺 token，不从新记录自动补造调用方 token。
- start/restart 在部署或启停前校验；冷热 url 部署拿到应用锁后再次校验，失败正常释放尚未执行的锁；start 派生的 update 原样传 token。update 在已持锁内核校验。
- restart 后续 owner 登记不再吞存储错误伪装成功。补充真实 SQLite recreation 测试：缺/旧 token 的 start/restart 拒绝且 runtime create/delete 调用为零；当前 token 的身份检查可通过，foreign owner 拒绝。新增测试尚未执行。
- 后续仍需：start/stop/delete/builder 全族统一持久化受理，request_id 及命令参数端到端传递，入口校验与最终原子受理之间的竞争约束。当前是请求字段与前置检查接入，不是完整代次并发安全验收。

### update 业务入口持久化受理（集中开发，未编译/执行测试）

- update 持锁内核已改为读取权威身份、以请求生命周期和元数据 revision 原子受理 Update 操作；移除先单独 record 元数据的路径。请求 JSON 只持久化摘要，敏感 env/secrets 不写入操作记录。
- 运行时参数携带本次 operation/executor/lifecycle 上下文；运行时写入前登记检查点。完成后落盘 Succeeded；存在未确认变更时记录 RecoveryRequired；只有运行时明确未发生变更才记录 Failed。PVC 已扩容后的后续准备失败不能以“无变更”清除状态。
- 新增业务组件测试源码：实际 SQLite 中的成功 Update 操作、传给 runtime 的上下文和元数据变更必须一致，终态后清 current operation。旧路由恢复测试补齐权威 owner fixture，保留原断言。
- 尚未完成公开 request_id 去重、operation_id 响应、可恢复命令参数保存及 Pending Update 恢复；start/stop/delete 其余控制路径仍需统一。该批未编译、未运行测试，不作为完整控制协议验收。

### update request_id 去重（集中开发，未编译/执行测试）

- UpdateAppRequest 增加 request_id；SQLite/PG 共享契约增加按 caller token 查询已受理 operation，包含合并请求的 alias。update 在运行时查询/修改之前核对已保存 intent 和当前生命周期，已成功的精确重试不再应用配置；失败/在途结果明确返回，不自动重发。
- 请求摘要使用 shared_types::encode_userapp_intent，递归排序 JSON 对象键、保留数组顺序和值类型；builder 配置摘要同步使用同一编码函数，避免 HashMap 插入顺序或 serde_json feature 变化导致假冲突。
- 增补业务回归源码：完成后精确 update 重试不增加 runtime create/patch 调用，相同 request_id 改 name 返回冲突且元数据不变；补充 alias 查询 app 隔离、对象排序/数组顺序契约。所有新增测试未执行。
- operation_id 对外响应、失败结果稳定错误码回放、恢复命令保存、其余控制入口和完整验收仍未完成。

### update 响应和请求结果查询（集中开发，未编译/执行测试）

- HttpResult 增加可选 operation_id；自定义 Serialize 仅在 Some 时输出，原 code/message/data/tid/success 保持。既有 Agent/permission 直接构造信封的调用补 None，不改变这些接口的实际 wire 字段。
- update handler 为未传 request_id 的调用生成独立 token，成功后核对持久化操作记录并回显 operation_id；原 AppRuntimeInfo data 不改形状。正式 update JSON 提取失败改走现有业务错误信封。
- 增加 GET operations/by-request（显式 user_id/request_id query），与按 operation_id/current 查询一起注册路由及 OpenAPI；服务层校验 owner 和当前生命周期，拒绝跨 owner 或新生命周期查询旧操作。
- 补充信封加性字段 wire 测试、已完成 update 按 request_id 查询/foreign owner 拒绝的业务测试源码。尚未编译或运行；其他控制响应、失败信封携带 operation_id 和安全 retry 等仍待接入。

### stop 控制入口持久化（集中开发，未编译/执行测试）

- shared_types 增加 UserAppControlRequest；正式 stop query 和旧 computer/pod/stop 的 prod 分支传递 caller owner/lifecycle/request_id，统一走受理路径并回显 operation_id。旧 prod 分支不再把任意 get_app 查询失败伪装为不存在，也不再忽略 caller owner。
- stop 在应用锁内校验生命周期、检查精确重试、登记持久化 Stop 操作，写入前记录 checkpoint；成功提交终态后标记物理操作已完成，响应状态查询失败不再留下本已完成的锁。未知变更保留 RecoveryRequired，未执行操作不靠超时接管。
- 闲置回收使用同一执行内核，但保留 wake_on_traffic=true；显式 stop 为 false。同步纠正原 stop OpenAPI 的自动唤醒描述。停止仍保留原有数据范围，不涉及 Agent PVC。
- 补充真实 SQLite + runtime adapter 回归源码：Stop operation 成功持久化、精确重复不再次 scale、foreign owner 拒绝、无 delete、流量唤醒阻断。尚未运行；计算资源条件 scale、未注册但权威不存在的幂等结果、取消/恢复检查点和实际 Docker E2E 仍待补齐。

### delete/purge-resources 持久化受理（集中开发，未编译/执行测试）

- DeleteAppRequest 增加 lifecycle_id/request_id，正式 handler 传入调用方身份，在服务锁内校验并持久化 DeleteCompute 或 PurgeResources。操作受理后捕获资源，破坏性操作前保存删除检查点；未知失败保留恢复状态，查询/捕获失败且尚无资源变更时可明确 Failed。
- 成功删除保留原 lifecycle，精确 request 重试在计算资源已消失时复用结果；已登记应用的计算面权威不存在也允许新删除请求幂等完成，但继续按类型捕获残余资源。查询错误不降格为不存在。未知应用身份的幂等处理仍待完成。
- 正式 delete 回显 operation_id，成功文本使用英文；purge 范围变化视为不同命令，不得复用相同 request_id。新增实际 SQLite + runtime adapter 用例源码覆盖无重复删除、不销毁 PVC、生命周期保留、计算已不存在与 purge 改参冲突。
- 删除实现从 lifecycle/update.rs 拆到 lifecycle/delete.rs，保留跨调用方的同一 teardown 内核，减少更新/删除混杂。旧锁回归补齐持久 owner fixture，原并发/version/无误删断言保留。
- 全部新增实现与测试仍未编译/执行；完整 delete/app 的请求身份、dev 删除检查点持久化、runtime lifecycle 条件写入和恢复授权仍待补齐。

### Full deletion caller fence and replay (source implementation; verification pending)

- Full deletion acquires the application operation guard before reading lifecycle identity; the resource helper receives the guard rather than reacquiring it.
- Formal delete/app accepts owner, lifecycle_id and request_id, emits the persisted operation ID, and retains a lifecycle tombstone. A missing lifecycle token cannot delete a recreated generation.
- Added source regression for owner rejection, exact request replay without repeat cleanup, and old/missing lifecycle rejection after explicit recreation; updated cancellation test to submit the required owner.
- Pending: consolidated compilation/tests, strict E2E cleanup receipts carrying owner/lifecycle, complete persisted dev snapshots and recovery, and extractor error normalization. No new passing runtime evidence claimed.

- Full/compute delete JSON extraction now returns validation errors through HttpResult instead of the framework rejection response. Docker cleanup receipts capture only application/owner/lifecycle identity labels; cleanup uses the captured lifecycle and stable request key, and refuses missing identity rather than looking up a replacement at cleanup time. Added source receipt regression. Remaining: audit every strict E2E creation path for receipt registration and run the consolidated gates.

### Runtime workload mutation preconditions (source; consolidated gates pending)

- K8s scale/restart capture the Deployment UID/resourceVersion before storage claim; wake/recycle policy patches also send captured UID/resourceVersion. Missing identity, terminating target, and unexpected ownership labels fail before workload mutation; actual PATCH 409 remains Conflict.
- Docker scale/restart inspect the production family and application ownership, then stop/start the captured physical ID. Restart no longer swallows unknown stop failure; only already-stopped is a safe no-op.
- Added source regressions for patch identity preservation and Docker cross-family/absent identity rejection. Remaining: actual API race tests, lifecycle execution context through these calls, full-operation rather than per-call captured identity, safe recovery and consolidated builds/E2E. These edits alone do not prove end-to-end lifecycle fencing.

### Workload API wire regressions and control identity semantics (source only)

- Added a bounded actual-HTTP K8s contract matrix for scale/wake/recycle, asserting UID/resourceVersion and intended payload in the real merge PATCH; covers successful commit and explicit 409/403 refusal. No cluster accessed, no test executed yet.
- Shared execution-context validation now separates application/owner/lifecycle identity from creation-configuration equality. Control intents can have a different fingerprint; creation reuse retains the stricter fingerprint condition. Missing/changed identity is still rejected. Added source regression.
- Next integration remains whole-operation stop context and captured target propagation; per-call CAS is not claimed to fence the entire operation.

### Stop execution context and captured resource receipt (source only)

- Added shared UserAppStopTarget and runtime capture/stop methods; unsupported runtimes fail explicitly rather than falling back to name-based writes.
- Controlled stop records operation/lifecycle/physical target in its durable checkpoint before changing the runtime. K8s validates resource lifecycle and submits replicas=0 plus wake policy in one UID/resourceVersion merge PATCH; Docker validates identity and stops by physical ID. Errors retain wake blocking pending recovery, with no name-based compensating write.
- Runtime production creation serializes an available execution context into Deployment annotations/Docker labels. The create coordinator still needs to supply this context; unlabeled existing resources need the planned explicit adoption flow. This batch is not deployable proof of the entire create/stop chain.
- Extended actual HTTP contract source to atomic captured stop; service source asserts persisted target matches operation and lifecycle. Consolidated compilation/tests remain pending per user instruction.

### Production creation durable admission (source only)

- Create requests now carry optional lifecycle_id/request_id. After locking, creation checks authoritative lifecycle, admits Create with metadata CAS, passes its claimed execution context to runtime, and records returned runtime identity before terminal commit.
- Exact request replay precedes runtime existence rejection; changed request intent conflicts without another creation. Unknown runtime mutation outcome retains durable failure/recovery evidence and the mutation marker.
- Empty-runtime and URL release creation propagate the caller lifecycle through the internal ensure path. Missing owner no longer silently becomes an empty creation owner.
- Added source regression binding runtime parameters to durable operation/lifecycle and proving exact replay does not create again. No compilation/test execution yet. Remaining: recovery payload and checkpoints, explicit existing-resource adoption, comprehensive admission of other controls, full gates and Compose/E2E.

### Update lifecycle capture before effects (source only)

- K8s updates validate execution context and resource lifecycle before PVC ensure/claim or configuration staging. The captured Deployment UID/resourceVersion travels to the final replace; the generation writer requires a captured target for lifecycle-aware updates and checks its name/kind/version.
- Docker updates validate the same lifecycle before preparation and delete only the captured container ID, preserving execution context on the replacement container.
- Added bounded real-HTTP source regression: old lifecycle receives Conflict on the first Deployment GET, before any PVC/configuration request. Existing generation-writer fixture calls explicitly retain their legacy no-context scope. Consolidated gates remain pending.

### Upper update coordinator target propagation (source only)

- Generalized the shared captured-target contract to UserAppMutationTarget for stop/update, with the same wire fields. Update captures target before its resize stage, persists the receipt, and transmits it in ContainerCreateParams.
- Docker/K8s consume the supplied physical target; params reject target/context mismatch or missing UID. K8s checks target kind/name/resourceVersion before PVC/config work. This fixes repeated workload selection; PVC expansion still needs its own captured storage identity contract.
- Added source regressions for cross-operation target rejection and equality of runtime target versus persisted update checkpoint. No compiler/tests run in this batch; actual Docker/PG/K8s-contract and Compose acceptance remain pending.

### PVC expansion conditional write (source only)

- Production PVC expansion now rejects wrong service family, terminating target, missing UID/version and name mismatch, including no-op size requests. The actual merge PATCH includes the captured UID/resourceVersion; a 409 remains Conflict with no automatic retry against a replacement volume.
- Added source regression for identity and family requirements; platform-generated shrink log is English.
- Remaining: tie the storage receipt to the operation lifecycle before the upper update coordinator effects and persist that receipt, plus actual HTTP expansion tests. Current change only proves source-level per-call conditional-write intent, not the full planned storage lifecycle.

- Added actual-HTTP PVC expansion contract source with bounded total runtime, asserting captured UID/version and requested quantity in PATCH and preserving 409 Conflict. Not executed yet; storage lifecycle receipt integration remains pending.

### Operation-bound storage expansion receipt (source only)

- Added shared UserAppStorageResizeTarget with operation context, physical PVC identity and captured capacity. New runtime methods default to explicit unsupported; Docker explicitly reports no capacity semantics.
- Upper update captures storage before marking mutation, persists the target and requested size with workload identity, then expands the captured target. K8s validates lifecycle annotations at capture and sends the recorded UID/version without rediscovering a replacement PVC.
- Added service source regression binding storage checkpoint to operation/lifecycle and requested capacity. Existing PVC quantity logic and conditional PATCH are reused.
- Production PVC lifecycle stamping for newly created resources and explicit adoption for existing unlabeled PVCs still need implementation; current strict capture intentionally rejects unlabeled storage. Recovery and consolidated gates remain pending. No runtime validation claimed.

### Lifecycle-owned PVC creation and reuse (source only)

- New builder/prod PVC creation carries application/owner/lifecycle and operation metadata in the initial POST. Existing PVCs require matching lifecycle/family and complete physical identity; terminating or unmarked PVCs are not silently adopted. Create 409 reads and validates the winner instead of assuming it is a terminating volume.
- Production storage claims reuse the admitted operation ID and validate lifecycle before conditional marking. Existing legacy second data volumes without identity remain explicit adoption work.
- Removed redundant platform pre-provision helper/module: runtime creation already receives resources.storage through ContainerCreateParams and ensures PVC before deployment. This avoids precreating an unmarked PVC before the context-aware runtime path.
- Added source reuse/adoption/family rejection regression. Actual POST/409 wire regression, explicit resource adoption, restart recovery, and consolidated validation remain pending; no current runtime pass claimed.

### PVC create/winner wire regression and reuse configuration (source only)

- Existing, newly returned and competing PVCs validate name, access mode, explicit storage class and sufficient capacity as well as lifecycle ownership. Expanded capacity is retained; insufficient capacity cannot silently satisfy ensure.
- Added bounded actual-HTTP source scenarios for initial POST identity/7Gi capacity, successful competing winner reuse, foreign lifecycle refusal, and incompatible access mode refusal. The conflict path must issue GET after POST 409, never delete the winner.
- Still uncompiled/unexecuted under the batch-development instruction. Existing resource adoption, broader recovery and full gates/E2E remain open.

### Controlled start admission and captured target (source only)

- Added durable Start admission with owner/lifecycle checks, canonical request deduplication and physical-target checkpoint before runtime start. K8s claims storage with this operation context and atomically patches wake policy plus replicas using captured Deployment UID/version; Docker starts by captured ID.
- The no-URL StartAppRequest flow propagates its caller lifecycle; the legacy internal start facade cannot silently authorize recreated generations. No compensating name-based wake-policy patch occurs after uncertain start.
- Added source regression for persisted identity, exact replay without repeat scale, and stale lifecycle rejection. Public request_id/operation_id response integration, automatic wake and restart convergence, captured storage claim recovery and actual runtime gates remain pending.

### Restart lifecycle admission and shared activation flow (source only)

- Start/restart share the operation admission, target capture, checkpoint and completion implementation; Restart has its own durable kind, so the same request key cannot switch semantics.
- K8s restart uses captured UID/version and a stable operation-ID template annotation; Docker stops/starts the same captured ID and does not select a replacement by name.
- Enhanced restart propagates the caller lifecycle. Legacy pod/restart production branch now carries owner/lifecycle/request ID, preserves domain errors and returns the admitted operation ID; platform log/message is English.
- Added source repeat/replay/kind-conflict and stale-lifecycle regression. Automatic wake, public enhanced-request idempotency, resource adoption/recovery and consolidated gates remain open.

### Traffic wake/manual-stop boundary (source only)

- Removed the obsolete traffic-overrides-manual-stop rule. Both local wake blocks and remote wake_on_traffic=false prevent a traffic-triggered scale write.
- Wake completion no longer removes a manual block, and a concurrent stop no longer triggers a compensating name-based scale0 that could affect a replacement resource.
- Updated the old opposite-semantics tests to the planned manual-stop contract and added remote manual-stop/zero-scale/no-flight source coverage.
- Automatic wake still needs the durable lifecycle coordinator and whole-operation identity/lock integration; the existing runtime-only scale leader remains an explicit pending item. No compilation or runtime tests executed in this batch.

### Automatic production wake coordinator (source only; batch development)

- Replaced registry-owned runtime scale/lease execution with a weak AppService coordinator reference attached during AppState construction. Missing/dropped coordinator fails explicitly; no runtime-only mutation fallback remains.
- Wake takes the application operation guard, validates current owner/lifecycle and authoritative manual-stop policy, admits a durable Start operation with a traffic trigger fingerprint, captures/checkpoints the physical target, and starts that target. Readiness checks verify its UID before and after the observed Running state.
- Remote query errors in ensure_running no longer return AlreadyRunning. A readiness timeout, transport error, replacement or panic cannot authorize another mutation; unfinished operation/lease remains fenced. No name-based compensating scale is issued.
- Migrated wake fixtures to actual SQLite admission and the actual AppService coordinator with an infrastructure runtime adapter. Panic/timeout regression now asserts no second writer; added missing coordinator, runtime query failure and replacement-Running rejection with RecoveryRequired checkpoint assertions.
- No Cargo compilation or tests executed. Format/diff inspection only; consolidated verification remains pending. Remaining scope still includes command payload/recovery/retry, resource adoption, complete public request identity, other controls and strict runtime/E2E gates.

### Start/restart orchestration preparation and request validation (source only)

- Consolidated start/restart post-activation overrides, PG alignment and response assembly into one helper. Removed restart's redundant metadata registration; it no longer changes metadata between separately admitted controls.
- Empty-runtime creation reports whether it created under the operation guard; env already supplied at creation is not sent through a second update/recreation. A concurrent winner discovered under the guard does not falsely mark this request's env as applied.
- Blank url is rejected before locking, identity registration or runtime work for both entrypoints; it previously allowed restart to skip the restart branch and also skip deployment.
- Formal start/restart handlers now catch JSON extractor rejections and return HTTP 200 business envelopes. Added actual-loopback HTTP regression source for malformed/missing-required-field/invalid-enum/wrong-content-type bodies, English messages and zero creation.
- Tightened empty-create regression from >=1 to exactly one create/parameter-history entry. Added zero-side-effect blank-url source coverage.
- Whole enhanced-request request_id/operation_id is still pending: forwarding a caller key to only one child operation would falsely imply whole-request deduplication. Composite durable admission must encompass deployment, overrides and optional credential effects before exposing that guarantee.
- No compilation or test execution in this batch; only format and diff checks. Full original implementation/validation scope remains open.

### SQLite canonical directory and file-alias protection (source only)

- Extracted local-filesystem/instance-file checks into a focused startup helper. Exclusive open now passes the canonical directory path to SQLx, not the originally configured alias path.
- Rejects symbolic/hard-link aliases for the database, WAL/SHM/journal files and instance lock; lock open additionally uses O_NOFOLLOW on Unix. Directory symlinks converge to the same instance lock. Reserved lock filename cannot be used as a database.
- Added source regressions for directory-alias contention/reopen retention, cross-directory database links, linked sidecar/lock sentinels unchanged, and invalid-database startup preserving the original bytes and releasing ownership.
- These checks assume a deployment-private directory, documented explicitly; they are not a sandbox against an actor replacing files concurrently. No user database was opened or modified by this batch.
- Verification remains deferred: no compilation or test execution; final consolidated gates must run all new SQLite cases.

### Public runtime query lifecycle/owner boundary (source only)

- The single-app HTTP query previously validated the user_id shape but ignored ownership when reading runtime status. It now calls an owner-scoped query with authoritative lifecycle reads before/after runtime observation; non-Active or replaced lifecycles cannot return a successful runtime payload.
- This remains a read operation: no identity registration, mutation admission, creation, or operation lease. Query extractor errors retain the HTTP 200 business envelope.
- Added source tests for rejecting another owner before consuming an injected runtime fault, successful rightful-owner lookup, and deterministic same-owner deletion/recreation during a barrier-controlled runtime read. The race test uses actual SQLite lifecycle transactions and rejects the stale result.
- Format/diff checks only; consolidated compilation/testing remains pending. Recycle-policy still requires complete durable admission and runtime/storage policy convergence, and is not claimed complete by this query fix.

### Durable start/restart/stop recovery inputs (source only)

- Added shared_types UserAppControlCommand for explicit/traffic Start, Restart and Stop wake policy. The command is stored in the same admission transaction as operation/lifecycle changes; it is non-secret and independent of release identity.
- Storage rejects mismatched command/kind and treats changed commands as changed request intent even if a supplied fingerprint is identical. Legacy records deserialize missing command as None; recovery must not invent input from current runtime state.
- Wired current start/restart, automatic wake and stop admissions to record their exact command; other operations explicitly have no replay command until their payload/reference design is implemented.
- Added common SQLite/real-PG contract source for SQL round-trip, exact request replay, changed trigger rejection, mismatched kind rejection, unchanged record on failed admission and legacy missing-input decoding.
- Automatic replay of these newly recorded control commands still needs the captured-target executor/recovery path; this is the admission prerequisite, not completed recovery. No compilation/tests run in this batch.

### Pending control recovery executor (source only)

- The existing startup/5-second scan now dispatches persisted control commands to AppService. It re-reads lifecycle/current operation under the application operation guard and ignores stale snapshots, claimed operations and missing commands.
- Added CAS claim of the original Pending record; no replacement operation or reconstructed caller parameters. Recovery checkpoints the captured physical resource and original command before runtime writes, using the same target APIs and stop boundary as live controls.
- Traffic-start recovery checks manual-stop policy and uses the shared captured-UID readiness observer. Explicit Start/Restart and Stop preserve their original semantics; uncertain writes retain RecoveryRequired and the runtime marker/lease.
- Added source regressions for exact original operation/lifecycle checkpoint identity, one execution despite repeated scan, no takeover after another executor claims Running, and manual-stop rejection before any runtime mutation.
- This does not claim recovery of Running/unknown outcomes, missing legacy commands, create/update/hot/purge payloads or complete safe-retry APIs. Those remain in the full plan. Consolidated compilation/tests still deferred.

### Stop rejection versus uncertain effects (source only)

- AppOperationError now retains structured RuntimeRequestRejection status/message across the runtime boundary. HTTP mappings retain existing conflict/backend codes; no new locale error code is introduced.
- Captured K8s patch keeps structured non-conflict 4xx classification, and captured Docker stop distinguishes definitive server rejection from timeout/connection failure (304/404 retain idempotent success).
- The single-request stop boundary clears its mutation marker and restores pre-request activity state only for RequestRejected. Live and recovered stop share this logic. Start/restart are multi-request operations, so a final rejected request alone does not release their earlier-effect boundary.
- Added source matrix for 403 versus 408/500, terminal Failed versus RecoveryRequired, activity restoration, one stop request, and next-writer marker availability/refusal. K8s 409 remains a separate captured-precondition conflict pending checkpoint-aware conflict reconciliation; it is not treated as a generic permission rejection.
- Format/diff checks only; no compilation or tests executed. Full lifecycle/retry/adoption/Compose validation remains pending.

### Accepted control failure correlation (source only)

- Structured AppError can now carry a verified operation_id while retaining the original code/message. UserApp HTTP error normalization preserves this optional identity; legacy errors without it keep their previous wire shape/status handling.
- Update, stop and compute-delete failures use an owner-scoped persisted request lookup, bounded to 5 seconds, to attach the admitted operation ID. Diagnostic lookup failure/timeout preserves the original business error and is logged, never converted to success.
- Added source wire coverage for HTTP 200 normalization retaining code/message/operation ID, and actual SQLite correlation tests for matching owner, wrong owner and never-admitted request.
- Corrected the start/restart malformed-body HTTP regression fixture to mount the production formal-route middleware; the earlier direct handler router would have tested raw AppError HTTP status instead of the product protocol.
- Composite start/restart, purge worker failures, request IDs during storage outages and other control responses still require integration. No compilation/tests run; consolidated gates remain pending.

### Full purge worker error identity (source only)

- Full-delete now flattens both worker JoinError and service failure into the same owner-scoped operation correlation path. Failure does not start another purge or erase the durable recovery record.
- Added source regression with an actual SQLite admission and injected compute-delete failure: error carries the original operation ID, state remains RecoveryRequired, compute delete runs once, development cleanup does not run, and the existing runtime resource remains.
- No compilation or tests executed; full plan remains incomplete. Format/diff checks are the only verification for this batch.

### Durable runtime-policy state foundation (source only)

- Added shared runtime policy and SetRecyclePolicy command/kind. The operation records requested fields; only Succeeded atomically merges them into the lifecycle's applied policy. Failed/RecoveryRequired leave prior applied policy and revision unchanged.
- Identical applied policy does not churn metadata revision. Explicit lifecycle recreation resets the prior generation's policy. Older stored records default to an empty policy so runtime creation defaults remain distinguishable.
- Added common SQLite/PG source contracts for admission not changing applied state, success/failure/uncertain outcomes, no-op revision stability and recreation reset.
- Policy handler/runtime projection and replay executor are not yet wired. Pending policy commands are explicitly excluded from the generic start/stop recovery executor until that integration exists; no successful policy feature claim is made by this foundational change.
- No compilation or tests executed; consolidated verification remains pending.

### Policy entry, runtime projection and Pending recovery (source only)

- Recycle policy now carries lifecycle/request IDs, validates ownership under the application operation guard, performs durable admission/deduplication, checkpoints the physical target, and commits applied policy only after projection succeeds. Handler returns the operation ID on success and correlates admitted failures.
- K8s projection is one UID/resourceVersion-protected metadata-annotations patch containing all supplied fields, without pod-template changes. Docker projection explicitly uses coordinator persistence instead of its legacy in-memory policy cache.
- Docker detail/list/startup stopped-state reconstruction reads committed policy from lifecycle storage. Runtime-cache guards are released before database reads. Bulk-list policy reads currently use per-app lookups and still need batching.
- Pending policy recovery now uses the same captured-target projection and SQL completion semantics. Successful Start/Restart/Stop also commit wake policy so Docker restart behavior matches explicit controls.
- Added source regressions for policy request dedupe, changed-content refusal, no runtime restart/recreate, SQL policy readback and original Pending policy recovery. Actual K8s wire tests and full restart persistence verification remain pending.
- Create/update initial policy reconciliation, removal of obsolete Docker policy cache consumers, bulk query optimization and consolidated gates remain open. No compilation/tests run.

### Batched applied-policy reads (source only)

- Replaced per-runtime SQL policy lookups in list and startup restoration with bounded 128-row lifecycle pagination. Single-runtime reads retain a single identity query. All pages must succeed before mutating the response snapshot.
- Runtime caching does not cache applied SQL policy, and its mutex is released before any policy query. This keeps policy updates visible on cached runtime status without holding a cache guard across database work.
- Added source regression crossing the 128-row boundary and changing the last-page policy while runtime data remains cached; the expected second policy is fresh and runtime list is fetched once.
- No compilation/tests executed. Original plan remains active, including create/update policy reconciliation, obsolete Docker cache removal, larger lifecycle/recovery work and final gates/E2E.

### Configuration-policy reconciliation and update-marker completion (source only)

- Added typed durable target policy to Create/Update admission, operation persistence and exact duplicate checks. The shared SQLite/PG transition publishes it only with successful completion; failed/uncertain outcomes preserve old policy and metadata revision. Other operation kinds reject this projection rather than allowing competing policy sources.
- Wired Create and Update using resolved runtime parameters. Fixed successful Update leaving its mutation marker unfinished after a successful terminal commit; marker completion now follows the commit.
- Added common SQLite/PG source regression for target persistence, changed-intent rejection, pending/failed/uncertain policy preservation, success merge and wrong-kind rejection. Added service source regressions for initial policy persistence, update readback over earlier dynamic policy, exact replay and subsequent operation-lock acquisition.
- Tightened Docker policy target validation to reject a mismatched application container name.
- Compilation, test execution, full Compose/E2E and the remaining plan implementation are still pending. No verification result is claimed from these test sources.

### Docker policy authority cleanup (source only)

- Removed the Docker runtime's volatile recycle-policy DashMap, its logical-name writer and raw-status overlays. Repository call-site search found no consumer of the old trait method outside runtime implementations. Docker now uses the fail-fast unsupported default for that legacy method; formal policy control continues through captured-target validation and SQL commit.
- Raw Docker runtime status intentionally omits coordinator-owned policy; AppService overlays committed lifecycle policy after reads. K8s legacy adapter is retained. No build/test execution in this batch.
- `cargo fmt --all` and `git diff --check` passed before this cleanup; rerun formatting/diff checks below does not count as behavioral verification.

### Recovery scanner fairness (source only)

- Replaced sequential per-operation waiting with an eight-slot recovery scheduler and process-local operation-ID deduplication. Kept SQL claims and resource leases as execution authority.
- Added bounded paginated discovery with a cursor retained across ticks, read timeout, Pending-only filtering and per-task panic isolation. Active executions continue being polled during storage discovery.
- Added deterministic source tests for a held operation alongside an independent completion, duplicate suppression, capacity, panic isolation and reuse of the freed slot. These are infrastructure fixtures, not AI responses.
- No compilation or test execution; full plan and consolidated gates remain pending.

### Full builder waiter deadline (source only)

- Wrapped both public builder ensure variants around a single absolute deadline, including the formerly unbounded metadata/runtime reads and final ownership verification. Existing internal lock and record waits use the same instant.
- Added explicit expired-before-poll behavior, deterministic virtual-time nested-budget tests and a worker-survives-waiter-timeout source test. Enabled Tokio test-util only in rcoder dev-dependencies.
- No compilation/tests executed. Stable timeout error correlation, complete deployment-operation integration, resource recovery and final environment gates remain open; this is not whole-plan completion.

### Builder timeout/conflict envelopes (source only)

- Added shared typed waiter timeout, a stable error code and three-language resources. Removed nested local-lock timeout wrappers so the public deadline consistently produces the typed result. Durable record waiting retains the accepted operation ID.
- Added common typed error mapping to pod ensure/keepalive and initial dev forwarding, retaining legacy gateway HTTP status and formal envelope normalization. String-shaped conflicts are not classified as real operation conflicts.
- Added source tests for anyhow context preservation, timeout/conflict code and operation identity, and rejection of text-only conflict classification. Updated the existing wait-timeout test to assert typed identity rather than a message substring.
- Compilation/tests and full acceptance remain pending. The outer deadline can return no ID before the waiter has one; this does not authorize a new operation.

### Persist development purge evidence (source only)

- Added shared serializable development-deletion and registry identities, with a required receipt accessor on captured deletion tickets. Updated the real implementation and all three known implementations including test fixtures.
- Both production purge and full application purge now persist production and development receipts before side effects. Capture rejects an application mismatch.
- Added a source regression requiring both physical and registration evidence to survive a failed development cleanup in RecoveryRequired. This closes the missing-evidence path, not the entire recovery executor.
- No compilation/tests run. Legacy dev pod stop still bypasses the full lifecycle protocol and remains an implementation item; the unrelated Agent removal-confirmation helper was not changed.

### Three-Compose persistence configuration guard (source only)

- Confirmed the three source configurations explicitly select SQLite and mount the whole `/app/data` directory. Synchronized deployment-repository SQLite documentation with the file/sidecar/lock alias and private-directory requirements.
- Added a read-only Compose JSON contract tool checking the explicit backend/path, writable directory mount, no nested sidecar mounts, one replica, expected absolute bind directory and project-scoped named-volume override. Resolution failures are nonzero and full interpolated configuration/credentials are not printed.
- Added source unit cases for wrong backend/path, file-only/read-only/foreign-source/nested mounts, replicas and shared external volume mistakes. No unit or Compose command executed in this batch; runtime persistence acceptance and consolidated gates remain pending.

### Typed deletion checkpoint binding (source only)

- Replaced both ad-hoc deletion JSON producers with one shared versioned checkpoint type, recording execution context and scope with the physical/registry receipts. Both paths validate before checkpoint commit and before deletion effects.
- Added authoritative-operation validation and mandatory identity/version checks. Added source regression assertions for wrong lifecycle, missing development scope, unknown schema version, absent K8s resourceVersion and old records missing context.
- This supplies validated recovery evidence; it does not implement lease takeover or unknown-outcome replay. No compilation/tests executed; consolidated gates and remaining plan work remain pending.

### Confirmed deletion boundaries (source only)

- Added typed monotonic deletion stages with unchanged evidence checks, persisted after each completed compute/storage/dev boundary. Shared the final two purge steps between both entry points. Failed development cleanup retains ProductionStorageRemoved and the original targets.
- Corrected K8s compute deletion acknowledgement being treated as completion: every captured kind now requires bounded authoritative disappearance. Failed observation and replacement UID stop cleanup. Full purge also processes residual captured compute resources when Deployment status is absent.
- Updated failed-purge source regression with the last-confirmed-stage assertion. K8s API fixture sequences must be updated for added GET confirmation during consolidated test preparation; no compilation/tests have run. Full recovery execution and remaining plan work are not complete.

### K8s deletion acknowledgement/observation contracts (source only)

- Added Foreground propagation to identity-bound application deletion so Deployment disappearance also waits for dependent Pod cleanup, while retaining UID/resourceVersion preconditions.
- Added bounded loopback HTTP scenarios checking DELETE payloads, a terminating intermediate GET, eventual 404, next-resource ordering, observation 503 and replacement UID. Error assertions distinguish observation failure from a later failed deletion attempt. No Kubernetes context or real cluster is used.
- No Cargo tests executed. Format/diff checks do not prove these new scenarios; consolidated execution remains pending with the rest of the plan.

### SQL deletion state-machine enforcement (source only)

- Added common domain validation rejecting premature success, missing/foreign evidence, skipped stages, target replacement and evidence erasure on failure. Terminal success requires the final stage already committed in an earlier transition.
- Added a shared SQLite/PG direct-storage regression for these bypass attempts and successful ordered completion. Updated storage/query recreation fixtures to supply an explicit empty-resource checkpoint and exercise all confirmation stages; they no longer synthesize deletion success without evidence.
- No Cargo gates executed. Whole-plan completion, recovery execution, deployment integration and real Compose acceptance remain open.

### Keep deletion traffic fences until terminal commit (source only)

- Removed unconditional old-route/activity restoration after runtime deletion errors and moved activity forgetting from compute teardown to the successful durable terminal boundary in both deletion entry points. Partial purge now keeps wake blocked.
- Changed deletion watch notification to send_replace, preserving the result when a caller has a handle but has not subscribed.
- Added source regressions for runtime delete failure retaining the fence, development cleanup failure retaining it after production storage removal, and late wake subscription after deletion. No compile/test execution; full acceptance remains pending.

### Restore deletion fences after process state loss (source only)

- Added durable lifecycle/current-operation checks to startup activity reconstruction. Deleting/Deleted and unfinished compute-delete/purge identities remain blocked, even when no compute runtime entry exists. Current-operation read errors/missing/inconsistent records fail reconstruction.
- Extended source regressions to clear process-local activity and rebuild from storage for a failed compute delete with a Running old runtime and a partial purge with absent compute. These exercise reconstruction, not actual process restart acceptance.
- No compilation/tests run. Deployment coordination, recovery replay, strict E2E and actual rebuild/restart persistence still remain to be completed.

### Consistent lifecycle/current-operation reads (source only)

- Replaced independent identity/current-operation reads during wake-fence reconstruction with a shared snapshot contract and one joined statement per page. Avoids reporting normal cross-replica completion as an inconsistent pointer while retaining genuine corruption errors.
- Added shared SQLite/PG source cases for paging, Pending linkage and cleared linkage after terminal commit, plus SQLite fault injection for a missing operation record. These are database fixtures, not AI mocks.
- No compilation or tests executed; remaining implementation and all final environment gates are still open.

### Cross-replica stale wake-block revalidation (source only)

- Removed local-only permanent rejection of blocked traffic requests. Coordinator policy/identity/admission checks now decide whether local flags are stale; refreshing flags happens only after the captured target checkpoint. Recovery follows the same order.
- Added an end-to-end component path through AppWakeControl for a stale block on a running application, followed by a real committed manual stop that traffic must respect. Updated the manual-stop recovery fixture to persist stop through the actual controlled API instead of only setting a process-local flag.
- Corrected the shared wake trait's stale documentation that previously claimed traffic could override manual stops. No compilation/test execution; real multi-replica validation and remaining plan work remain open.

### Cached running observations cannot authorize wake success (source only)

- Removed the activity registry fast return from cached replica counts. The existing coordinator now validates lifecycle, policy and captured resource identity for every ensure_running success. Remote routing probe caching remains unchanged.
- Added deterministic source regressions for a cached running observation followed by remote manual stop, and a running runtime without a durable application identity. Updated the detached-coordinator expectation; no retry or success criteria were relaxed.
- Compilation and test execution remain deferred until implementation is complete. These source cases are not runtime or multi-replica acceptance evidence.

### Builder recovery holds scheduling capacity through worker completion (source only)

- Changed builder worker launch to return its completion handle. Recovery awaits actual execution and checkpoint cleanup; ordinary HTTP waiters still detach and cannot cancel the shared operation. Worker interruption/checkpoint errors reach the recovery observer after existing reconciliation handling.
- Added a deterministic scheduler fixture with a spawned worker held by a one-shot barrier: handing off the task cannot free capacity, completion does. This is component source coverage, not a full restart test.
- Compilation and execution remain deferred; builder deadlines, complete recovery commands and final environment acceptance are still outstanding.

### Forward lookup convergence and typed error preservation (source only)

- Reused one builder error adapter for initial and repeated ensure, preserving timeout/conflict operation IDs. Added source wire assertions for the legacy 502 envelope.
- Bounded the full dev resolution path, including probe and re-ensure, by the configured total budget. Removed the forwarding layer stale-observation registry overwrite and stream shutdown; coordinated ensure rechecks current state.
- Production detail failures retain their typed code; Docker lookup failures no longer collapse to absence, and a temporarily unavailable address reports 503. Corrected obsolete wake documentation.
- No compilation or tests executed. Full HTTP/multipart and strict E2E acceptance remain outstanding.

### Explicit retry entry for durable unclaimed control commands (source only)

- Added shared validated retry request, service/trait implementation, HTTP route and OpenAPI registration. Requires owner/lifecycle/revision, preserves operation ID, observes existing success without executing twice, and rejects claimed/uncertain or input-less operations. HTTP cancellation detaches the worker.
- Added component source cases covering owner/lifecycle/revision rejection with unchanged stored state, successful repeated retry executing once, and refusal to take over a claimed operation.
- This completes the entry for existing basic control commands only. Builder/create/update/hot/delete/storage recovery and unknown-result reconciliation remain incomplete; compilation, tests and environment acceptance are deferred.

### Durable deletion scope and unclaimed deletion recovery (source only)

- Persisted production delete/purge scope and expected resource version as shared commands. Extracted one captured-resource executor used by initial deletion and Pending recovery, retaining the original operation, UID/version deletion and typed progress checkpoints. Successful deletion does not publish a new runtime policy.
- Added source regressions for compute-only versus resource purge (including development cleanup and retained lifecycle), repeated recovery without duplicate deletion, and stale expected resource version with zero destructive calls.
- No compilation/tests run. Full DeleteApplication/storage-specific commands, recovery after mutation and final environment gates remain incomplete.

### Pending full lifecycle deletion recovery (source only)

- Persisted DeleteApplication command and reused the full purge executor for unclaimed recovery. Recovery now permits Deleting only for the linked full-deletion operation of that same lifecycle; basic controls still require Active.
- Added source cases for successful full deletion and failed development cleanup: exact original lifecycle/operation, typed final or partial checkpoint, retained Deleting state and wake block on failure, and no repeated execution from an old Pending scan.
- No compilation/tests executed. RecoveryRequired reconciliation, storage-specific recovery, remaining deployment work and final gates remain incomplete.

### Controlled storage destruction and Pending recovery (source only)

- Replaced uncoordinated dev destruction and post-effect production handling with owner/lifecycle-checked durable admission, exact request replay, captured shared resource evidence and one executor. Retained actual production cleanup scope and lifecycle identity; removed production unwrap/expect in the replaced dev branch.
- Added HTTP lifecycle/request fields, correlated operation responses, JSON rejection handling and cancellation-independent workers; Pending recovery uses the original shared command. SQL now rejects premature success, changing identities, wrong owner/scope and skipped cleanup stages.
- Added source component scope/ownership/exact-replay cases; upgraded common SQLite/PG storage lifecycle fixtures to required evidence and premature-success assertions. No compilation or tests executed.
- Storage content clear, uncertain-result reconciliation, complete deployment input persistence and full final gates remain outstanding.

### Storage content clearing filesystem semantics (source only)

- Consolidated content clearing into shared_types and switched both app_manager Docker clearing and file-server-userapp workspace reset to it. Root I/O errors no longer become fake absent success; root symlinks are rejected and child links are unlinked without inspecting their targets.
- Added source fixtures for missing roots, retained root directories, invalid file roots, external directory links and dangling links. These do not prove protection against concurrent root replacement or lifecycle coordination.
- Formatting/diff checks only; no compilation or tests. file-server-userapp changes require the final agent-runner image build. Full clear admission/task quiescence/identity protection and other final gates remain outstanding.

### Workspace reset waits for actual worker completion (source only)

- Added per-application weakly retained workspace read/write leases. Build and dev start/restart retain read leases through actual worker completion; app-files operations use the same boundary. Reset invalidates generations/cancels tasks, releases the generation mutex before awaiting exclusivity, then stops processes before clearing.
- Reset executes independently of its HTTP observer, preventing cancellation from dropping its lease while filesystem work continues. Failed process termination aborts clearing.
- Added deterministic source assertions that a Cancelled event cannot release a still-running worker lease, reset excludes new work, and unrelated applications remain independent. No compilation/tests run; full platform clear admission, other workspace writers and actual subprocess/HTTP regressions remain outstanding.

### Additional workspace writers share the reset boundary (source only)

- Connected file mirror modifications/uploads/import/generation, project confirmation and workspace initialization to the same per-app workspace lease as reset. Commands and dependency installations execute in independent workers holding the lease across observer cancellation.
- Added a real files_update handler source regression: with the reset write lease held, polling the handler cannot create its file; after release it writes the exact expected bytes. This is source coverage only, not execution evidence.
- Formatting and diff checks passed. No compilation/tests run; platform clear admission/identity, external writer boundaries, remaining recovery and final environment gates are still open.

### Durable platform storage clear admission (source only)

- Added controlled clear request/trait/handler, lifecycle and owner validation, exact request replay, operation correlation and cancellation-independent execution. Preserved distinct clear scope; production clear now uses captured K8s resources rather than unqualified PVC deletion.
- Added shared target receipts and SQL validation for identity/scope, immutable targets, captured-before-effects and confirmed-before-success. Failed JSON/negative container results are errors.
- Added source production tests for foreign-owner rejection, original-operation replay with one storage mutation, retained lifecycle, no compute deletion, and existing-compute rejection recorded without storage effects. No compilation/tests run.
- Remaining clear work includes physical HTTP target verification, directory replacement fencing, and durable replay/reconciliation; other deployment/recovery tasks and final gates remain open.

### External workspace writes retain builder ownership on uncertainty (source only)

- Fixed read-only deletion tickets being dropped after an external workspace clear was submitted: they now enter an explicit mutation state before sending. Unsupported adapters fail before dispatch; timeout/transport errors retain the builder fence. Explicit successful completion checks lease release and propagates release errors.
- Added source regressions for response uncertainty retaining ownership, confirmed release exactly once, refusal to reuse a completed ticket, and failed release preserving uncertainty. No compilation/tests run.
- Physical HTTP endpoint identity verification and directory replacement protection are still separate unfinished work; the full plan and environment gates remain open.

### File-server instance fence for workspace clear (source only)

- Added shared internal clear probe/target/request DTOs, a read-only target endpoint and mandatory per-process instance matching before reset effects. Platform persists the instance identity, submits it and checks the success echo. Route registration and generated OpenAPI include the probe.
- Added actual handler source tests: a previous process identity leaves files intact, the current identity clears and echoes correctly, and missing/empty identity is rejected. No compilation/tests run.
- Initial physical runtime-to-endpoint binding, directory replacement safety, complete recovery and final environment gates remain unfinished. Builder/file-server image rebuild is required for this internal protocol change.

### Reset acknowledgement and template/skill writer completion (source only)

- Verified actual router middleware excludes app-files paths and does not wrap successful bodies. Added shared typed reset acknowledgement and strict confirmation predicate; platform decoding and generated response schema now consume the same type.
- Added source tests for missing/negative/stale/empty acknowledgement identities, and a registered-router probe/reset sequence checking rejected reset preserves contents and matching reset clears with the expected wire response. Added the required tower test dependency.
- Connected template initialization and skill push to the workspace activity lease, held by independent execution workers through observer cancellation. Input upload remains outside the workspace lease.
- Compilation and tests remain deferred by user instruction. Physical runtime-to-endpoint binding, root replacement protection, remaining lifecycle recovery, strict regression coverage and final build/environment gates are not complete.

### Runtime-backed workspace endpoint binding (source only)

- Added shared endpoint observation, captured-ticket delegation and concrete Docker/K8s runtime implementations; unsupported adapters reject before workspace writes. SQL clear evidence now includes the physical endpoint and validates its direct base URL.
- Platform probes the process instance between matching uncached runtime observations, uses direct IP and disables redirects/proxies; mismatch/read failures stop before external mutation begins.
- Added source Docker/Pod identity matrices and K8s API regression checking replacement/403 performs only the initial workload GET and never queries the replacement Pod. No compilation/tests run.
- Explicit adoption of existing unlabeled resources remains open; no unsafe fallback was added. Directory replacement safety, remaining durable command/recovery work, comprehensive regression mapping and final environment gates are still incomplete.

### Directory replacement fencing (source only)

- Added separate durable directory receipts and live descriptor leases, captured absence semantics, iterative descriptor-relative Unix clearing, and root revalidation. Docker clear holds all handles across its SQL checkpoint; file-server reset captures its root before waiting for workers.
- Added deterministic source regressions for replacement after capture, creation after absent capture, and exact replacement between root validation and handle traversal. Existing root/link preservation cases remain. No compilation/tests run.
- New direct rustix dependency uses already locked 1.1.4; final Cargo resolution/check will update dependency edges. Non-Unix clear explicitly unsupported, not silently unfenced. No resource-size limits or unsafe code added.

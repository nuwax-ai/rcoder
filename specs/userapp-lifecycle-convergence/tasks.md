# 实施任务

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

# userApp 修复执行与证据

基线 d886b67，开始时工作区干净。只有实际执行成功的验证才能标记完成。

- [x] T01 严格 E2E 门控、选择、run 报告聚合及设施测试
- [x] T02 R01/R06 资源归属补偿与真实 CAS，先红后绿
- [x] T03 R05 注册表权威查询与错误传播
- [x] T04 R02/R03/R04 热部署准备、身份、原子受理与 env
- [x] T05 R07/R08 SSE 快照及取消启动提交
- [x] T06 R09/R10 临时资源与下载/解压预算
- [x] T07 R11 正式 HTTP/i18n 契约与兼容测试
- [x] T08 Docker 故障与 A/B 制品黑盒回归
- [x] T09 workspace 与独立 app-cli 全量 gates
- [x] T10 本地 builder/runtime 构建及 make dev-hot，运行产物校验
- [x] T11 严格 make test-e2e + make test-e2e-compose，全量报告与资源清理
- [x] T12 最终差异审查、命令证据及未覆盖层级

## 验证记录

以下为按时间保留的执行记录，早期“待跑”条目以最新证据为准。K8s 实机验收由用户后续部署新镜像后进行，不计入本轮已通过。

### 2026-09-12 实施中证据（尚未完成最终验收）

- R07：`terminal_after_subscription_is_delivered` 旧实现红灯 `/tmp/rcoder-repair-sse-red.log`，修复后绿灯 `/tmp/rcoder-repair-sse-green.log`。必须保留订阅快照终态游标，不能读之后的 live terminal 代替。
- R01/R06：真实 kube 客户端连接本机 HTTP API 适配器的相同 resourceVersion 竞争测试通过，日志 `/tmp/rcoder-repair-k8s-contract.log`。两个更新仅一个提交；失败方仅对自己的 CM/Secret 发 UID 条件删除。不是实机 K8s 验收。
- R04：app_manager 99 个测试通过，包含 `hot_env_change_rejected_before_runtime_mutation_equal_env_removed`，证据 `/tmp/rcoder-repair-component-tests.log`。同次 file-server-userapp 60 通过、1 个旧 HTTP 400 断言失败；该断言已按批准契约改为 200，并保留专用错误码断言，待重跑。
- R08：启动提交与 stop 使用同一代次边界，barrier 回归 `stopped_generation_cannot_commit_after_preparation` 已在上述 60 个通过项中执行。
- app-cli 前一轮 97 项通过；新增 OS 准备租约、取消期间 blocking 解压持有租约及恢复期受理保护后，独立 Clippy/测试正在重跑。
- 严格报告校验 Python 4 项测试通过；缺报告、未终结、无硬断言、失败断言伪装 pass 都被拒绝。
- 新增真实 Docker A/B/故障套件 `compose_userapp_faults`，已纳入默认 userApp 必测集合，尚未执行。
- 构建前核对 `start-up{,-common,-docker-extra,-k8s-extra}.sh` 与 build-agent-docker/build_config 副本一致。
- 尚未执行本轮 builder/runtime 镜像构建、dev-hot 与最终严格 E2E；不得把以上组件通过记成全链路通过。

### 后续门禁进度

- Kubernetes 特性 Clippy 通过（`/tmp/rcoder-repair-clippy-k8s.log`）；默认全 workspace Clippy 通过但报告一处 `needless_option_as_deref`，已修正，最终冻结门禁需重跑确认零新增警告。
- app-cli 98 个测试通过，包含活跃准备租约与过期目录回收；最近补充启动清扫与恢复状态字段后仍需最终重跑。
- K8s 两项契约回归通过：同版本竞争与提交响应丢失；新增第三项并发 create 契约待跑。
- 全 workspace 首轮在 rcoder 文档守卫失败：旧守卫强制所有 userApp 声明 4xx/5xx。已改为正式入口 200＋HttpResult/SSE，旧兼容/流量入口保留原错误状态要求；聚焦回归进行中。
- runtime 首次构建因 named context 未应用根 `.dockerignore`，主动中止。新 `docker/build-app-runtime.py` 仅整理 Cargo 源码，排除凭据、target、node_modules 和报告，实测上下文 8.4 MiB。
- builder/runtime 正在构建；中途组件修复之后必须确保最终二进制取自最终源码，不得使用早期构建快照验收。

### 本地运行回归（第一轮红灯证据，不是最终验收）

- 全 workspace：`/tmp/rcoder-repair-final-workspace-tests.log`，1825 passed / 0 failed / 35 ignored，77 个测试组；环境门控忽略不计入严格 E2E。
- 默认与 Kubernetes Clippy：`/tmp/rcoder-repair-final3-clippy.log`、`/tmp/rcoder-repair-final3-k8s-clippy.log` 均通过。
- SSE 五项真实 handler 流回归：`/tmp/rcoder-repair-sse-final.log`；覆盖订阅后终态、回放中终态、越界游标、慢消费者 lagged 和重连回放。
- K8s 三项真实 kube 客户端／本机 API 适配器测试通过：`/tmp/rcoder-repair-final-k8s-contract.log`；含相同版本竞争、并发 create、成功提交丢响应。没有操作集群。
- `make dev-hot` 首次成功，`/tmp/rcoder-repair-dev-hot.log`；进程与编译输出 SHA256 同为 `b944b5581503ebe54d62ab7aff8740561770704d5e43207384fcbb712b30713c`，健康检查成功。后续消息规范修改已触发第二次构建，最终身份仍须复核。
- 第一轮 builder/runtime 构建成功；镜像快照早于恢复迁移保护修复，不能用于最终冻结验收。
- 严格报告测试 5 项通过，新增“部分成功终态不能掩盖必需步骤缺失”；固定步骤目录在 `tests-e2e/tools/contracts.py`。
- 真实 Docker 双引擎故障回归 run `bcf7599ec7e2470199362eaac8331da5`：两引擎均通过 10 类 prepare 失败／旧内容健康／无临时残片断言；builtin 暴露启动失败仍标 Running，supervisord 暴露恢复时重复执行旧迁移。报告保留为红灯证据。
- 对上述红灯：server 编排失败不再沿用开发模式的部分启动成功语义；新增 Recover 动作，两引擎恢复时跳过迁移，恢复结果不声称逆向数据库迁移。新镜像重新构建中。
- 严格入口清理增加 case 独占名称与已登记容器 ID；取消/超时终止本测试进程组，聚合报告将未完成场景记为 aborted；收集身份、状态及脱敏日志后才定向回收。
- 正式 HTTP 错误测试改为同时断言 HTTP 200、具体业务错误码及英文消息；已删除路由和旧 TS 入口继续断言原有 404/400，不能统一放宽为任意成功状态。

### 恢复修复候选验证

- 候选镜像 `rcoder-review-recovery-candidate:local` 使用本次重新构建的 app-cli Linux 二进制，仅用于提前验证恢复修复。
- Run `896a9e8b397a41d0a7a9215c394a6ea2` 两个场景均通过全部必需断言（builtin/supervisord；含 A/B 内容切换、并发拒绝、失败恢复、迁移次数、容器 ID 和清理）。
- 该 run 期间基础测试源码有修改，来源冻结门禁正确返回 2；**不计作最终严格验收成功**。
- 最终生产源码指纹：`279153871c58ecfc468beab504120e5e7f3369250fad57f3962d9acc56f4b2bf`，与最终 builder 构建日志一致；后续测试修订不改变生产源码。
- 第二次 dev-hot 完成，运行 `/proc/1/exe` 与 `/app/bin/rcoder` SHA256 均为 `278dbae83009e0ff3e38d7ba775edfd62a889fcac2142aa115069e533f7fba71`。
- 本地基本套件第二轮仅剩状态错误码断言差异：存储清理保护返回 `ERR_INVALID_STATE`，已保持该专用码而非强改服务端或接受任意错误。

### 开发目录补查（最终冻结前）

- 新增 `invalid_package_cleans_its_staging` 在旧实现红灯：失败 ZIP 留下 1 个 staging，`/tmp/rcoder-repair-dev-staging-red.log`。
- 准备与激活分离：独占 TempDir 与 OS 租约由 blocking worker 持有，hygiene 获取同一租约；激活进入任务代次提交边界。`stale_preparation_neither_promotes_nor_leaks` 证明准备不换目录、活跃清扫不误删、失效代次不激活且清理残片。
- file-server-userapp 65 项通过：`/tmp/rcoder-repair-dev-staging-green3.log`。此修改之后重新构建镜像和重跑 workspace 门禁，前一轮镜像身份不再代表最终源码。
- 探索 run `6180faac756547989fc5e700970c236e`：24 个 dev + 3 个 build-rules 场景通过；基本套件暴露旧断言/fixture 问题已修；完整部署七服务构建成功但生产流量未就绪，仍在定位，不记为通过。

### 工具链实际失败与门禁

- `3784c6cfd500467b9bf776ca1725943d` 七服务构建成功，冷部署 Python `SPAWN_ERROR`；实际制品 pydantic_core 为 cp311，runtime Python 3.13。运行中读取服务日志确认 `ModuleNotFoundError`，清理前诊断与容器身份已保留。
- 旧基础镜像配对红灯 `/tmp/rcoder-repair-toolchain-red.json`；Make 前置门禁 `/tmp/rcoder-repair-base-guard-red.log` 在编译前返回 1；显式 trixie 基础镜像配对绿灯 `/tmp/rcoder-repair-toolchain-base-green.json`。
- builder 重建命令 `make docker-build-agent-runner AGENT_BASE_IMAGE=rcoder-computer-agent-runner-base:latest-arm64 AGENT_TOOLS_CACHE_KEY=1789145228`，日志 `/tmp/rcoder-repair-build-agent-trixie.log`。
- 第三次 dev-hot 已完成，运行二进制 SHA256 `20b2590e83dae295d0cfcab072f5e13cdc690f8d4d9cec547b0368fea67fd824`。同期开启的探索部署 run `a26f9bb03d5547119488514b610d82a3` 被重启中断，按 aborted 保留；后续验收禁止并行重启服务。

### 最新组件门禁（生产源码 5951774c9cc502d32916e369026f02ee6c46e94b87f10ceed096e2541a6d4005）

- workspace `cargo fmt --all --check`、默认及 Kubernetes 特性 Clippy 通过：`/tmp/rcoder-repair-final5-clippy.log`、`/tmp/rcoder-repair-final5-k8s-clippy.log`。
- `cargo test --workspace --locked` 最终退出 0，1829 passed / 0 failed / 35 ignored，77 个测试组：`/tmp/rcoder-repair-staging-workspace-tests.log`。使用不可达本地 URL 让环境门控测试跳过，不计作严格 E2E 证据。
- 基础严格 E2E run `2c454b55e15a4e978f6e6b1e94cdf186` 通过、退出 0、无源码漂移。
- 测试工具 9 项通过：`/tmp/rcoder-repair-tools-final3.log`。

### standalone 链接语义（R09/R10 延伸，真实红灯驱动）

- 真实 run `ad7ac2a67fa94d298b7dc6c962c4d9e3` 保存了 `verified-artifact.zip` 及服务日志：Next 把 node_modules/next 内的链接文本当 JS，报 `Unexpected token '.'`。聚合 raw-copy 经 zip 8.6 的 options() 丢失 S_IFLNK。
- 修改前两项组件红灯：`aggregate_preserves_dependency_symlink_metadata`（`/tmp/rcoder-repair-aggregate-link-red.log`）、`deploy_preserves_internal_dependency_symlink`（`/tmp/rcoder-repair-deploy-link-red.log`）。旧 runtime 双引擎 run `5b4fb106c55b43e5b14f29e5316d85ff` 也无法运行依赖链接的 A 制品；该 run 期间源码在修改，记录为探索红灯而非冻结验收。
- 合并阶段显式 add_symlink；开发及生产解压使用 shared_types::archive_links 统一链接路径/链解析约束，普通条目不得穿过已有链接，链接最后写入并验证真实文件系统解析；操作准备目录验证前不对应用暴露。支持安全的可选悬空依赖，拒绝绝对、越界、循环及大小超限链接。
- app-cli 本地 build 复用同一受限部署解压器；开发解压同时保留安全可执行位。
- 组件已通过：app-cli 99 项、file-server-userapp 67 项、共享链接四项（含真实文件系统 Unicode 别名）、file-server ZIP 三项。日志 `/tmp/rcoder-repair-link-appcli3.log`、`/tmp/rcoder-repair-link-userapp3.log`、`/tmp/rcoder-repair-link-contract4.log`、`/tmp/rcoder-repair-link-fileserver2.log`。
- 双引擎 Docker A/B 制品现在必须从内部符号链接加载内容，另增 link-escape/link-cycle 准备失败回归。builder/runtime/main 正重新构建；此前 5951774... 镜像和门禁不是此批修改的最终证据。

### 链接修复后的冻结门禁（源码 53e1726039230e4b808ae553c8fb3459a4cdce3cd4c27f88485d9d468d67b03e）

- workspace fmt/default Clippy/Kubernetes Clippy 通过；全量测试退出 0：1835 passed / 0 failed / 35 ignored，77 组。证据 `tests-e2e/reports/userapp-repair-evidence/rcoder-repair-links-*.log`；忽略项不作为严格 E2E 通过。
- app-cli own Cargo.lock 独立 fmt/Clippy/test 通过，99 passed。K8s `conditional_tests` 三项通过，真实 kube 客户端仅访问本机适配器。
- make dev-hot 完成，运行 `/proc/1/exe` 与 `/app/bin/rcoder` 均为 `ed671800b6d4ad360e7655d7844c831c738d3b11c775f0f7530de46cb7537a72`，健康检查成功。
- builder 镜像 `sha256:01ef2f773f2acecdd47c1a6380386040ce67598fe71ad529c177bbd4f33cb8d5` 已完成；runtime 最终镜像尚在构建。
- 候选 runtime（新 app-cli + 既有语言层）严格 run `208d189375dc4bd292dcfe7eb518039c`：builtin/supervisord 全部必需断言通过、无源码漂移、定向清理成功，退出 0。含链接制品 A/B、link-escape/link-cycle，仍须最终镜像全套验收。

- 最终 runtime 构建成功（`/tmp/rcoder-repair-build-runtime-links.log`），工具链配对通过，证据 `tests-e2e/reports/userapp-repair-evidence/toolchains-final-links.json`。按计划顺序再次执行 dev-hot 后开始完整严格验收。

### 最终完整验收

- `make test-e2e`：run `f24f39d55adb4987b330da992ae9f192`，31/31 passed，378 项硬断言，33 条清理记录成功，退出 0。
- `make test-e2e-compose`：run `474a9b4e5a304589bb76bf4d8a05f64b`，54/54 passed，317 项硬断言，58 条清理记录成功，退出 0。真实 SSE/chat/会话调用未使用 mock AI。
- 两个 run 的执行前后工作区指纹均为 `2f5c78d8d7c723e12ae03c2b88467492f2f0c7bb0174d845dbb1dce4d38e3c2b`。无失败、跳过、中止、缺报告、源码漂移及清理错误。
- 最终 runtime：`sha256:ff40b26fa8f3d9ccf31a5798978b18a77de30f91878ea6ca8d0e06282bc4da89`；builder：`sha256:01ef2f773f2acecdd47c1a6380386040ce67598fe71ad529c177bbd4f33cb8d5`。
- 按镜像构建后顺序执行的最后一次 `make dev-hot` 成功：主进程与 `/app/bin/rcoder` SHA256 均为 `c7d0284e4a0f19a1f8e403833940fa96e4dd0e54dbef3a4d39b319bf159830f1`；完整验收后再次核对一致。main 使用 dev-hot 二进制，不声称重建了主容器镜像。
- 七服务全链冷部署、热部署、各服务实际流量通过；保存的 `verified-artifact.zip` 中保留 57 个符号链接。双引擎 A/B 内容、操作与制品身份、故障保活和恢复断言全部通过。
- 最终容器集合与第一套完整验收开始前完全一致：既有容器无丢失，本轮无新容器残留。三个外部仓库状态与基线一致；`git diff --check` 通过。
- 最终证据归档：`tests-e2e/reports/userapp-repair-evidence/`。验收后只更新 Spec/Plan/Task 报告类证据，不改变生产与测试源码；真实 K8s 集群、npm 发布、镜像推送、tag 均未执行。
- 完成标记依据上述实际命令与固定回归映射，不表示所有条目都有修改前红灯，也不扩大为实机 K8s 或全域分布式事务保证。实现边界见 report.md 与 plan.md。

### 用户追加授权：app-cli npm 发版

用户要求 app-cli 逻辑修改后升级版本并打 tag 发布 npm。本次在完成上述本地验收后，将 Cargo.toml 与独立 Cargo.lock 的 app-cli 版本从 0.3.1 升级为 0.3.2；独立 fmt、Clippy 和 99 个测试再次通过。此次版本字段升级晚于镜像验收，镜像证据仍对应当时的 0.3.1 版本标识，业务代码相同。准备推送 app-cli-v0.3.2，由 release-app-cli.yml 构建五平台并发布六包；发布结果须另行核验。

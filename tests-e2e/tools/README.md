# 严格 userApp 回归入口

整体入口、分组、验收与清理逻辑见 [测试逻辑与验收流程](../architecture.md)；本页维护场景命令和前置条件。

owner 丢失的容器内专项回归使用 `owner_recovery.py`，入口与验收边界见 [开发环境 owner 恢复](../../docs/userapp-dev-owner-recovery.md)。它不替代下列完整 Compose/K8s 套件。

Source 凭据恢复专项使用 `make test-e2e-source-credential-recovery`，先串行构建当前 Linux app-cli/file-server-proxy 并绑定源码、二进制 SHA256，再验证真实 PostgreSQL 凭据、脱敏回执、同卷容器替换、新 Source 版本、失败日志、Stop/Start 和迁移去重。它只清理自己创建的容器、保留工作卷；缺工具、源码漂移、业务失败或缺断言均为失败。报告默认为 `tests-e2e/reports/source-credential-recovery-*.json`，不代表 RCoder/Java 或远端 K8s 验收。

## UserApp 核心场景聚焦回归

这些场景均无 LLM，已加入固定套件、报告身份与必测步骤登记。可以分别运行，不必每次执行完整 E2E：

```bash
# 原平台闲置回收 → 同卷重建 → 编译/HTTP → Stop/再Start，含准确 owner 强杀。
CARGO_BUILD_JOBS=2 make test-e2e E2E_SUITE=compose_userapp_dev E2E_FILTER=userapp_dev_idle_recycle_owner_recovery

# Rust RCoder冷部署A/热部署B/容器Restart/readiness操作身份与数据保留。
E2E_PROD_BUILD_RECEIPT=/absolute/frozen-paired-build.json \
  CARGO_BUILD_JOBS=2 make test-e2e E2E_SUITE=compose_userapp_faults E2E_FILTER=userapp_prod_readiness_contract

# 实际模板脚本+真实pip：连续构建、平台停容器后原卷重建、缓存与两层ZIP。
E2E_TEMPLATE_SOURCE_DIR=/absolute/frozen-userapp-workspace-template \
  CARGO_BUILD_JOBS=2 make test-e2e E2E_SUITE=compose_userapp_build_rules E2E_FILTER=userapp_python_dependency_cache_survives_builder_recycle
```

prod 用例先核验构建输入，随后自动创建本 case 的私有 Compose RCoder/Turso、工作目录、loopback 端口，结束与中断后均按捕获身份清计算，保留数据。构建阶段生成的 `E2E_PROD_BUILD_RECEIPT` 格式如下；不能在验收时给任意旧镜像现算哈希冒充本轮构建：

```json
{
  "version": 1,
  "source": {"origin_head": "本次源码提交", "worktree_sha256": "本次冻结输入内容摘要"},
  "rcoder": {"image_id": "sha256:不可变镜像ID", "binary_sha256": "镜像内/app/bin/rcoder的构建摘要"},
  "runtime": {"image_id": "sha256:不可变镜像ID", "app_cli_sha256": "镜像内app-cli的构建摘要"}
}
```

源码摘要使用 `prod_readiness_contract.source_identity`，与严格启动器的冻结清单口径一致，支持 `E2E_SOURCE_ROOT`、`E2E_INPUT_MANIFEST`、`E2E_ORIGIN_HEAD`。构建后固定镜像及二进制摘要，运行时逐一核验。前置缺失或来源不匹配立即失败。

Python用例冻结真实模板脚本并记录模板版本、提交及文件摘要；用有效的小 wheel 和本 run 的 Docker HTTP索引执行真实pip，比较索引请求、deps库存、stamp、mtime与解释器。全链缺数据、缓存重新下载或ZIP夹带缓存都不能通过。模板源仍需显式准备；工具单测不等于组合链已验收。

### 显式 CephFS 跨节点 owner 互斥

该实验只在获准的个人集群执行，独立于默认 Compose 套件，不会被普通 `make test-e2e` 触发：

```bash
K8S_LOCK_CONTEXT='<个人context>' \
K8S_LOCK_NODE_A='<Ready节点A>' K8S_LOCK_NODE_B='<不同Ready节点B>' \
K8S_LOCK_IMAGE='<单平台镜像@sha256:摘要>' \
K8S_LOCK_BUILD_RECEIPT=/absolute/app-cli-lock-build.json \
  make test-e2e-app-cli-cephfs-lock
```

回执字段为 `schema_version:1`、`image`（上述digest引用）、`app_cli_sha256`、`source_commit`、`source_digest`。镜像必须包含本次编译的app-cli、Python3.9+；节点内核支持pidfd，测试凭据有读取PVC/PV权限。每个Pod实际镜像、app-cli摘要、实际节点与CSI类型都核验。

`source_commit/source_digest` 使用与上方prod构建receipt相同的 `origin_head/worktree_sha256` 口径；首次访问集群前匹配当前冻结源码，结束清理后再次核验未漂移。旧镜像配套旧回执不能冒充当前源码；可通过 `E2E_SOURCE_ROOT` 指向构建时冻结的目录。

验收要求首owner持锁且实例不变、竞争节点没有独立管理监听，准确终止首holder后新实例接续并保留哨兵。只按捕获Pod UID/resourceVersion清计算，namespace及所有PVC保留；`--cleanup-volume` 明确拒绝。`test-e2e-app-cli-k8s-lock` 还包含RBD与同Pod容器重启，额外提供首节点免密SSH映射 `K8S_LOCK_NODE_SSH=node=host:user`。

**仍需后续新增的断言**：prod旧容器仍Running时的可控Restart屏障、迟到Ready在真实观察链被丢弃，以及K8s RCoder原入口的编译/HTTP组合链。当前case的代码和工具自测不能替代最终配对镜像、真实Compose或CephFS集群执行结果。

## 真实 agent 项目适配（独立容器）

`userapp_project_repair.py` 默认运行两个原始项目：`code/` 下的完整旧工程，以及未配置平台 manifest 的 Node HTTP/shared monorepo。使用镜像已有的 OpenCode CLI（可为 `nuwaxcode`），注入当前模板系统提示词及两份完整技能，让真实模型自行适配；不借用共享 RCoder 服务，也不预先替 agent 写适配结果。

```bash
python3 tests-e2e/tools/userapp_project_repair.py \
  --app-cli tests-e2e/reports/_bin/app-cli-linux \
  --file-server-proxy tests-e2e/reports/_bin/file-server-proxy-linux \
  --template-root ../userapp-workspace-template \
  --report /private/tmp/userapp-project-repair.json
```

输入必须是本轮 Linux ELF 二进制，并与 `--image` 的架构匹配；默认镜像 `dev-rcoder-agent-runner:latest` 会先解析成不可变 image ID。两个副本各有独立 app_id、容器和持久卷，容器使用 builtin 引擎；无数据库 fixture 不要求 PG。可分别用 `--case misplaced-nested`、`--case imported-source` 和不同 `--report` 并行执行。默认每 case 的 agent 总预算 900 秒、构建/运行阶段预算 180 秒，可用对应参数调整。

真实执行会调用 LLM。配置只从 `--env-file`（默认仓库 `.env.local`）读取 `LLM_API_KEY`、`LLM_BASE_URL`、`LLM_MODEL`，采用 OpenAI-compatible provider；key/完整 endpoint 不打印、不进入 Docker argv 或持久卷。`OPENCODE_CONFIG_CONTENT` 经 docker exec stdin 传入子进程环境，CLI 的 HOME/XDG 位于容器 tmpfs。报告保存脱敏工具轨迹、prompt/技能/二进制哈希、文件 diff、原根 validate JSON、实际构建、任务 GET/SSE、逐服务启动成功日志以及 HTTP 业务断言。仅模型回答“已完成”、任务 completed 或端口存活均不能使测试通过。

首次可加 `--preflight-only`，只验证当前二进制、`--pure` 的提示词加载、完整技能、未适配 fixture 及其失败任务 GET/SSE，不发送 LLM 请求。此模式即使退出 0，也明确记录 `preflight_only=true`、`ai_executed=false`、`success=false`，不能当作真实 agent 验收。

JSON/JSONL 先按结构脱敏再序列化，普通日志和 diff 使用文本脱敏。技能/源码阅读需要实际返回正文，bash 读取还要求 exit=0；不能用文件名出现在命令或最终回答中替代读取证据。Stop 后保存容器内 socket 的确切 errno，仅 ECONNREFUSED 证明业务入口关闭；Docker exec/观测错误不算关闭，同时核验容器仍运行、原管理实例仍在且 desired=stopped。

所有结果必须满足固定必测项；失败、超时、缺配置或不完整轨迹均非通过。结束时按捕获的容器 ID 和本次 run 标签移除自有容器，保留卷、源码及报告；不清其他服务，不删 owner 状态或锁，不伪造模型输出。此脚本覆盖 agent → 本轮 file-server/app-cli 的容器内链路，不替代 RCoder/Java 转发、K8s 或完整部署验收。

`make test-e2e` 运行基础、dev、build-rules、Docker 热部署故障与完整制品部署链。
`make test-e2e-compose` 保留共享 chat/SSE 场景并补齐 build-rules。

选择套件：`make test-e2e E2E_SUITE=compose_userapp_faults`。
选择场景：`make test-e2e E2E_FILTER=userapp_hot_deployment_builtin_contract`。
场景清单（含每场景最近一次 verdict 与耗时）：`make test-e2e-list`。
三份登记一致性秒级校验（写完登记立即验证，不必等整轮跑红）：`make test-e2e-check`。
报告目录修剪（默认 dry-run；`APPLY=1 DAYS=3 KEEP=30` 可调）：`make test-e2e-prune`；
`APPLY=1 DEDUPE=1` 时额外把各 run 的冻结二进制副本收敛为 `_bin/` 内容寻址硬链接。
选择未命中、必要环境缺失、skip、aborted、缺报告、硬断言失败均返回非零。
基础设施用例不要求 LLM key；真实 chat 场景仍需有效模型配置。

每次调用使用唯一 run ID，报告固定写入 `tests-e2e/reports/<run-id>/`。
`manifest.json` 保存计划，`summary.json` 聚合实际结果；每个 libtest 场景独立进程与目录。
每个 result 含 `duration_s`（场景墙钟耗时），summary 另含 `total_duration_s` 与
`slowest_cases` top-N——迭代定位慢场景直接读 summary，不必翻 jsonl 时间戳。
源码指纹覆盖 tracked 和未忽略的 untracked 文件内容；镜像记录不包含容器环境变量。
冻结测试二进制按内容寻址存 `reports/_bin/<sha256>/`，run 目录内为硬链接——
`test_binary_sha256` 仍是实际执行二进制的内容指纹，同源码复跑不再重复占空间
（历史各轮源码不同则二进制唯一，磁盘回收靠 `make test-e2e-prune` 按龄修剪）。
独立执行 `cargo test --workspace` 仍采用环境门控，不代表严格 E2E 验收通过。

`hot_contract.py` 使用真实本地 Docker 镜像 `dev-app-runtime:latest`，启动本次 run 标签限定的容器。
A/B 制品由本机 HTTP 服务器提供；受控阻塞制造并发窗口，断言操作 ID、manifest ID、实际响应、容器 ID。
冷启动通过 env 下发 A 的独立操作与代次；热部署 B 后重启同一容器，必须保留 B 的操作、制品与真实响应，不能被不可变的 A 启动 env 覆盖。成功断言同时要求协议 4、匹配的操作/请求/制品/代次及已持久化的部署成功状态。
准备失败保持 A；切换后失败保持失败，测试通过显式重新部署 A 完成手动回滚，不要求自动恢复旧版本。
故障覆盖 404、截断、SHA、坏 ZIP、缺 lock、读取空闲超时及不安全链接。
按用户要求，app-cli 不再设置下载／解压容量及条目限额；B 制品超过测试中保留的旧 APP_DEPLOY_MAX_* 配置，必须成功部署。路径和符号链接安全检查仍保留。
它不调用或替代 AI，不能作为真实 LLM 测试证据。

运行前必须先构建 builder/runtime，并确认 Compose 的 `RCODER_RUNTIME_IMAGE_DIGEST=dev-app-runtime:latest`。
生产集群与远程开发集群不属于默认入口；K8s API 契约测试使用本机适配器。
修复编号与行为不变量见 `specs/userapp-review-repair/spec.md`，实际验证证据见该目录 `tasks.md`。

`contracts.py` 固定热部署与完整链的必需断言名：早退后写 pass 也会因缺步骤失败。
`suite_cases.json` 独立固定每个测试套件的场景成员，并核对断言与报告身份登记。新增场景须同步三份登记；完整套件中删除既有测试会失败，不能通过动态发现缩减必测集合。显式 `E2E_FILTER` 只要求匹配的已登记子集，整体未命中仍失败。
取消/超时会结束本次测试进程组，尚未完成的计划场景记为 aborted。
清理记录位于各 case 的 `resources/`；只处理随机 case 名称空间及创建响应登记的不可变容器 ID。
已有容器不因名称前缀相似而被删除。清理失败和诊断采集失败独立计入结果。

完整部署前会探测实际镜像的 Python SOABI、架构和 Java 版本，拒绝旧 builder 基础镜像与 runtime 配对。构建可通过 `AGENT_BASE_IMAGE` 指定本地已验证的基础镜像；也可先运行 `make docker-build-agent-base` 重建默认基础镜像。用 `python3 docker/verify-userapp-toolchains.py --builder <image> --runtime <image>` 单独检查，输出包含不可变镜像 ID。

## 显式真实 K8s userApp 验收

用户授权个人测试集群后，可运行不依赖 Docker/LLM 的专用入口：

```bash
make test-e2e-k8s-userapp \
  TEST_K8S_SSH=soddy@192.168.32.131 \
  RCODER_URL=http://192.168.32.131:30295 \
  E2E_PINGORA_URL=http://192.168.32.131:30435
```

仅支持 `nuwax-k8s-test`，检查入口属于实际节点。通过 SSH 转发直连不同 rcoder Pod；构建真实 A/B 静态制品，验证任务/SSE、冷部署、容器内热部署、失败保旧、停止后新 Pod 恢复 B 和资源清理。无需改动日常 Compose 配置。

该入口独立生成 `reports/<run-id>/summary.json`、断言、请求、源码指纹和 K8s 镜像/UID 证据。所有必要断言必须通过；并发失败可记录后继续验证后续链路，但整体仍返回非零。它不覆盖原有 Agent/SSE LB、真实 AI 或七语言工具链全套场景。

行为规范和真实结果见 `specs/k8s-userapp-acceptance/`。测试前必须已有用户对目标集群的明确授权。

## Turso Compose configuration contract

After implementation freezes, run `python3 tests-e2e/tools/turso_compose_contract.py docker/docker-compose.yml` and repeat for both build-agent-docker deployment Compose files. Repeat each with `--named-volume` and with `--data-directory /absolute/isolated/path` for the bind override. This command only resolves configuration, uses no Docker service mutation, and does not print the full interpolated environment. It does not prove Turso startup, migrations, image features or persistence across container recreation; those require the isolated runtime acceptance. Tool unit cases join the existing `test_*.py` discovery.

### userApp 持久化严格门禁

`make test-e2e` 的 `userapp` 组现在同时包含：

- `turso_storage_contract`：固定清单中的 Turso 组件契约，使用真实临时 Turso 数据库，验证事务、CAS、去重、生命周期和数据库重新打开。不会启动或重建日常 Compose。
- `pg_storage_faults`：ProjectStore 生命周期、重启/回源会话索引、跨副本同步、选主互斥，以及 UserApp 独立连接 CAS/锁等待/事务空闲超时、活动时间和 Preview 的真实 PG 契约；使用本轮独占的 PostgreSQL 17 Compose 项目。userApp 用例以 `--exact --include-ignored` 显式执行，不接受 ignored 或零用例。

可用 `E2E_SUITE=turso_storage_contract make test-e2e` 或 `E2E_SUITE=pg_storage_faults make test-e2e` 聚焦。两者均不要求 LLM key。入口冻结测试二进制、保存哈希及逐项输出；固定用例缺失、实际执行数不为 1、失败、中止或缺少报告均不通过。组件测试清单在 `storage_contract_cases.py`，外层必经断言在 `contracts.py`。

这些证据不能证明容器挂载或实际进程强杀恢复。三份 Compose 配置解析、运行中 `/app/data` 挂载、容器重建后 HTTP 查询和资源身份保留仍须单独验收；详见 [持久化回归映射](storage-acceptance.md)。

`turso_compose_runtime` 是单独的真实 Compose 重建验收，已纳入严格 userapp 组。它需要最终构建证据中的 `E2E_TURSO_BINARY_SHA256`（64 位小写 SHA-256）和可选 `E2E_TURSO_RUNTIME_IMAGE`；不能现场读取任意旧镜像哈希再把它当成本轮构建证据。默认镜像为 `dev-master-rcoder:latest`，实际按解析后的不可变 image ID 启动。

该场景顺序验证三份 Compose 的 Turso 配置，并分别派生仅含 rcoder 的隔离服务：随机项目名、动态 localhost 端口、run 专属数据及工作空间、禁用两层自动回收，使用镜像二进制。私有配置副本不进入报告，结束后精确删除；Turso 数据和脱敏证据保留在 run 报告目录供核查。它创建真实 builder 后通过 HTTP 和 SQL 内容核对重建持久化，最后验证错误 Turso 配置非零退出。若创建结果不确定且无法完成定向清理，将失败并保留控制面及数据供恢复；不能假报清理成功。

`userapp_concurrency_contract` 固定运行 23 个确定性组件测试（创建截止时间、取消观察者、晚订阅、恢复执行槽位、旧代次启动阻止、清空实例身份和不确定租约）。它不执行真实进程强杀，也不证明三个崩溃窗口恢复；单副本实际 HTTP 首开扇入由 `turso_compose_runtime` 单列覆盖；跨副本首开仍需 K8s 验收；`docker_lifecycle_crash` 实现前两个 SIGKILL 窗口。新增终态 receipt 扫描属于组件证据，不能将 legacy marker 的 native SIGKILL 测试作为新协议第三窗口验收。

冻结RCoder源码快照运行三份Compose配置验收时，可用 `E2E_BUILD_AGENT_DOCKER_ROOT` 指定另行冻结的镜像仓库配置目录，默认仍为RCoder相邻的build-agent-docker。该目录必须含两份原始Compose配置；应将其内容纳入本轮输入清单与指纹，不指向运行中会被修改的工作树。

### 宿主机与 Compose 共用 UserApp 计算流程

`userapp_dev_compute_shared_contract`（Compose）、`host_userapp_dev_compute_no_llm`（宿主机 Docker Published）与 `host_k8s_userapp_dev_compute_no_llm`（宿主机本地 K8s）共用 `tests-e2e/src/common/userapp_compute.rs` 的 HTTP 顺序与操作终态断言。物理探针分别验证 Docker 工作区挂载与所有 Published 端口、K8s Pod 换代与 PVC UID 保留。三个场景都不调用 LLM，也不构建七语言制品。

```bash
E2E_SUITE=compose_userapp E2E_FILTER=userapp_dev_compute_shared_contract make test-e2e-compose
RCODER_URL=http://127.0.0.1:<宿主机独立端口> make test-e2e-host-userapp
KUBECONFIG=<隔离 kubeconfig> TEST_K8S_NS=<rcoder-* namespace> RCODER_K8S_NAMESPACE=<同 namespace> RCODER_URL=http://127.0.0.1:<宿主机独立端口> make test-e2e-host-k8s-userapp
```

本地 K8s 启动器不运行 Docker 盘点和清理；它只在显式隔离 namespace 中清理归属本次测试的普通 agent 资源。UserApp 删除由 RCoder 完成；若操作结果未知，启动器报告残留供恢复，不绕过生命周期删除 PVC。远端 K8s 的 `make test-e2e-k8s-userapp` 继续覆盖 Helm 与真实集群拓扑，尚未与这条 Rust 宿主机场景合并。

OrbStack 首次创建 UserApp PVC/Pod 可能超过默认 90 秒，运行前可给宿主机 RCoder 设置 `RCODER_USERAPP_ENSURE_TIMEOUT_SECONDS=240`（测试 HTTP 预算 300 秒）；详见 [宿主机部署说明](../../docs/deployment/host.md)。

### 手动 app-cli 与平台交替控制

```bash
CARGO_BUILD_JOBS=2 make test-e2e E2E_SUITE=compose_userapp_dev E2E_FILTER=userapp_manual_owner_multi_process_control
```

一个场景复用同一临时 builder 和轻量 Python HTTP 服务：独立进程启动 app-cli owner → RCoder 停服务 → 新 HTTP 客户端重复停止 → 新 app-cli run 客户端转交启动，两轮后最终停止。核查真实 HTTP、同一 owner/监督父进程、唯一活动执行代次、容器与工作区保留，并清理本次捕获的容器 ID。无 LLM、无七语言模板构建。详细阶段及镜像身份在场景目录 `manual-owner.json`。

本场景验证同一项目运行目录上的多客户端顺序控制，不代表多 RCoder 副本并发、源码/制品目录切换、控制器 Stop/Restart 或 K8s 验收。

### 闲置回收后的 owner 恢复与重新构建

```bash
CARGO_BUILD_JOBS=2 make test-e2e E2E_SUITE=compose_userapp_dev E2E_FILTER=userapp_dev_idle_recycle_owner_recovery
```

`idle_owner_recovery.py` 创建独立的 Compose RCoder、Turso 数据库、网络和工作目录，仅该实例采用 60 秒闲置阈值、5 秒扫描间隔。使用真实清理器连续两轮判闲置，不用 `docker rm` 代替被测回收动作，也不改日常 Compose 配置。无 LLM、外部依赖下载或七语言构建。

准备阶段先确认真实 PG 初始化及登录完成，期间通过公开 keepalive 保活并核对容器 ID 未变化。首轮构建故意持续 75 秒，构建期间不发 keepalive，验证异步任务执行能阻止误回收；任务完成后才开始真正闲置的计时。PG 初始化超时是前置失败，不跳过 PG 预检。失败时保留 PG 日志。

第一轮通过真实监督控制信封、代次、工作区和锁文件核验合并 owner，使用 Linux pidfd 精确挂起该进程，确保真实回收后必须消费绑定原容器、代次和挂载的退出证据，不能只依赖优雅关闭碰巧成功。

两轮回收之间还注入同容器故障：核验 owner、代次锁与仍运行的命令守护范围后，用 pidfd 精确终止合并 owner，保留磁盘上的 Running/Draining 和真实命令清理链。不会对其父进程 agent_runner 发送信号，也不把未知拓扑作为跳过注入的理由。随后经 RCoder Restart 重新构建、检查 HTTP 新内容，再 Stop/Start；必须自动收束旧代次为 Quiescent，且容器 ID 不变、RestartCount 仍为 0。此步骤不会清除登记或伪造清理回执。

同一场景依次验证：独立 app-cli owner → RCoder 构建并登记 → 闲置回收物理容器 → 原文件和旧 owner/journal 保留 → RCoder 重新 ensure → 直接 Start 恢复管理面并重新构建 → Stop/重复 Stop/Restart → 第二次真实回收 → 先 Stop 恢复管理面再 Restart 构建。两轮回收放在同一场景，避免先成功 Start 掩盖 Stop 自身的恢复问题。必须有不同的容器与 owner 身份、相同生命周期和挂载、构建计数实际递增，以及 HTTP 返回各轮新内容；任务 `completed` 本身不能使场景通过。

为覆盖原 K8s 故障中“整卷保留”的条件，私有 builder 显式把 `APP_CLI_STATE_ROOT` 放在其持久日志挂载下。Docker 默认 `/home/user` 父目录可能不持久化，不能让 journal 随容器消失而掩盖恢复缺陷。此场景不证明 K8s PVC/STS/RBD 本身通过，个人 K8s 仍需对应验收。

镜像默认是 `dev-master-rcoder:latest` 和 `dev-rcoder-agent-runner:latest`。运行前应重建与当前源码配套的 RCoder、agent_runner（内嵌 file-server）和 app-cli；可用 `E2E_IDLE_RCODER_IMAGE`、`E2E_IDLE_BUILDER_IMAGE` 指定已构建的独立测试镜像。测试固定解析后的 image ID，不在运行期间更新镜像；报告记录二进制 SHA-256，旧镜像的结果不能作为当前源码验收。

报告在场景目录的 `idle-owner.json` 和 `idle-owner-runtime/`：保留每轮任务、owner、容器/挂载、回收日志及失败原因。退出或被启动器中断后只清理核验归属的测试容器/网络，保留数据库、工作区、构建计数与诊断证据，不清空登记、不手工改终态。

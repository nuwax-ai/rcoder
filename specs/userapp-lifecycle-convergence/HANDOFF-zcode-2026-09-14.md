# RCoder 移交 zcode：完成本地验收、提交发布与个人 K8s 验收

日期：2026-09-14。用户要求移交执行；本文不是计划完成证明。

## 1. 任务和授权

继续完成 `specs/userapp-lifecycle-convergence/spec.md`、`plan.md`、`tasks.md` 的完整目标。主体实现已落源码，但最终验收未结束，不能只运行一个组件就宣布完成。

用户最新授权顺序：

1. 完成本地 Docker Compose 测试；通过后 git commit 并 push。
2. 在 `/Users/soddy/Documents/git-workspace/build-agent-docker` 执行：
   ```sh
   make setup k8s-helm-rcoder-version-publish ENV=test
   ```
3. 构建成功后读取实际新版本，在个人测试机 `soddy@192.168.32.131`（SSH 免密）部署 `nuwax-k8s-test`。
4. 若部署引起 MDS/存储 Pod 异常，先取事件、日志和存储状态，按原因恢复，再运行真实 K8s userApp 回归。

仅测试环境获得授权。禁止操作 `nuwax-k8s-prod`；不删除既有应用数据、Agent PVC 或重置存储集群。测试资源按本次 run ID 和真实资源身份清理。无需再次询问常规编译、提交、测试环境发布的权限。

## 2. 当前仓库与进程（先重新核实）

- 主仓 `/Users/soddy/Documents/git-workspace/rcoder`，分支 `feature-userapp`；移交时 HEAD `fa7c9f7`，提交标题 `docs: unify agent guidance and deployment testing workflows`。
- 主仓移交时约 201 项工作区变更，包含此前会话和用户提交后的工作。不要 `git reset --hard`、整体覆盖或未经核对 `git add -A`。
- 构建仓分支 `test`，HEAD `dae4c10`；`k8s/helm/nuwax-platform/VERSION` 当前 `0.1.265`。这不是待部署的新版本。
- 构建仓未提交：`.gitignore`、`build_config/rcoder/start-services.sh`、两份 Compose，以及两个部署目录内新增的 SQLite 环境示例、说明和 named-volume override。保留其他会话变更。
- 本轮没有提交、push、打 tag、构建/推送 K8s 镜像或访问测试集群。app-cli 本轮无改动，不空发 npm。

### 最终串行验证队列已结束：全部 exit 0

证据目录：`/tmp/rcoder-lifecycle-consolidated-gates/`，完整命令及终态见 `results.json`。

- fmt：通过。
- 默认 workspace、K8s、PG+SQLite storage 三组 Clippy：全部通过，均带 `-D warnings`。
- workspace 全量测试：86 个目标汇总 2094 passed / 0 failed / 35 ignored。包含普通环境门控；不能当成严格 E2E 验收。
- K8s runtime：180 passed / 0 failed / 5 ignored。
- 存储普通测试：109 passed / 0 failed / 1 ignored，真实 PG 显式契约另有独立通过证据。
- Python 工具：90 passed。

原 Codex session `53655` 已确认 exit 0，原 PID 不再用于等待。没有启动镜像构建、提交或部署进程。接手核对工作区无新修改后，可进入本地实际构建和严格 E2E，不必无条件重复已完成门禁。

统一环境：
```sh
export CARGO_TARGET_DIR=/tmp/rcoder-userapp-lifecycle-target
export CARGO_BUILD_JOBS=2
```
Cargo 串行执行。不要干扰用户其他仓库的编译进程。

## 3. 已落地的主体实现

- shared_types 统一 userApp 身份、生命周期、操作、条件写入、资源绑定及真实 lease receipt 契约。
- SQLx SQLite/PG 权威 userApp 存储、迁移、请求去重、私有执行输入、资源绑定、检查点、终态与恢复扫描。
- SQLite WAL/FULL、foreign_keys、5 秒 busy timeout、单连接池、数据目录独占检查；启动失败不回退内存。Agent 存储策略不变。
- builder 首开等待收敛，共享截止时间，取消等待不取消已受理创建；显式 adoption、stop/restart、Pending 恢复及有证据的最终恢复。
- 生产 create/update/deploy/hot/delete/storage 通过统一持久化受理；未知远端写不自动重放或清锁。
- 安全目录清理、实例 nonce 清理协议、有限错误请求体 drain；不恢复下载/解压容量和条目限制。
- 三份 Compose 数据挂载、构建同步检查和严格 E2E 报告/冻结场景集合。

最近由实际测试发现并修复的缺陷：

1. builder 创建就绪后先写完成证据，再提交终态；恢复只检查原物理身份和就绪，不重建。SQLite/PG reserve 校验权威 owner。
2. 显式 retry 复用 builder Pending/final 两类恢复函数；未抢到执行权返回 false/冲突，不伪称执行成功。
3. 流量唤醒不能在 Ready 前清除待确认标志；保留不确定状态和新旧操作保护。
4. update 最终检查点追加结果，保留原 target/storage_target/requested_storage_size，防止恢复凭据丢失。
5. staging 文件锁不能仅靠 File drop：复制文件描述符可能继续持锁。增加显式 unlock 和 Drop 兜底，取消时仍先清理 staging 再解锁。确定性回归已先红后绿。
6. 新 HTTP/OpenAPI 文档、环境标签、路由登记及字段文档已补齐；内部 clear-target 不暴露在主公开文档。

## 4. 已有证据及限制

优先读取最新统一队列，以下是此前实际终态，不冒充最终冻结验收：

- `/tmp/rcoder-lifecycle-test-affected-2.log`：app_manager 174 通过、rcoder 271 通过。该轮 file-server-userapp 有 1 项准备锁失败，已随后修复。
- `/tmp/rcoder-lifecycle-staging-lease-red.log`：复制文件描述符的确定性锁回归失败。
- `/tmp/rcoder-lifecycle-staging-lease-green.log`：修复后 file-server-userapp 76 通过、0 失败、0 忽略。
- `/tmp/rcoder-lifecycle-test-docker-kubernetes-final-3.log`：runtime 180 通过、0 失败、5 项既有 ignored。不能把 ignored 算作严格验收成功。
- `/tmp/rcoder-lifecycle-test-storage-combined.log`：存储 109 通过、1 项显式 PG ignored。
- 真 PG17：`/tmp/lifecycle-pg-409f10bf7a274a7591aea20d934494a7/`，7 个原项目生命周期用例及 userApp 真实事务/重启契约通过，10 项记录全 true（含环境和清理）；测试 Compose 项目已定向清理。不是只跑了跳过的 PG 测试。
- Python 工具最终全套 90 通过；严格目录已包含唤醒/检查点/文件锁回归。
- 三份 Compose `config --quiet` 均通过，尚未证明实际 SQLite 重建持久化。
- 旧任务账本中的早期 34/34、55/55 Compose 报告对应早期二进制，不能证明当前代码。

新增严格目录见 `tests-e2e/tools/concurrency_contract.py`、`storage_contract_cases.py`、`suite_cases.json`。真实组件故障注入不是 mock AI。真实 AI 场景仍需真实配置。

## 5. 接手后立即执行的剩余工作

### A. 收尾统一门禁

先读取已经完成的队列结果。后续修改导致失败时保留日志，修实际问题，不放宽断言、不加无条件重试消除失败。成功后核对每项 exit code 和测试汇总；普通 workspace 环境门控不替代严格 E2E。

以下命令已全部执行通过，供复核与后续变更时选择性重跑：
```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p docker_manager -p rcoder --features kubernetes --all-targets --locked -- -D warnings
cargo clippy -p rcoder-storage --features pg,sqlite --all-targets --locked -- -D warnings
cargo test --workspace --no-fail-fast --locked
cargo test -p docker_manager --features kubernetes --lib --locked
cargo test -p rcoder-storage --features pg,sqlite --lib --locked
python3 -m unittest discover -s tests-e2e/tools -p 'test_*.py'
```
如后续修改 app-cli，使用其独立 Cargo.lock 另验；当前没有其改动。

### B. 本地实际构建与严格验收（未执行）

核对构建脚本和跨仓复制源，保留用户修改，再执行：
```sh
make docker-build-app-runtime
make dev-restart
make dev-hot
```

- 本地 Docker context 是 `orbstack`，现有开发服务来自主仓 `docker/docker-compose.yml`，HTTP 通常为 `http://127.0.0.1:8090`，接手再次核实。
- 当前配置映射：`/Users/soddy/Documents/git-workspace/rcoder/docker/data/rcoder` → `/app/data`，数据库 `/app/data/userapp.sqlite3`。挂整个目录，WAL/SHM 也保留。
- 显式核对 builder/runtime 镜像、运行容器 image ID、rcoder 二进制 SHA256、健康和实际 SQLite 挂载。
- 基线现有容器身份在 `/tmp/rcoder-lifecycle-local-baseline/containers.json`；不要清理它们来省事。

执行：
```sh
RCODER_URL=http://127.0.0.1:8090 make test-e2e
RCODER_URL=http://127.0.0.1:8090 make test-e2e-compose
```
严格入口必测缺失、skip、aborted、零场景、报告缺失和清理失败均不能通过。

SQLite 隔离测试还有必要前置：`E2E_SQLITE_RUNTIME_IMAGE` 和 `E2E_SQLITE_BINARY_SHA256`。镜像内 `/app/bin/rcoder` 与 dev-hot 改过的现有容器二进制可能不同，不能混用哈希或去掉校验；如要测试同一个 hot 产物，显式制成本次专属本地镜像。

三份 Compose 的测试是“原配置契约检查＋隔离 rcoder 同镜像启动/重建持久化”，不等于启动了现有两套完整业务服务。按计划在隔离目录和端口验证，不能重启用户整套部署。严格入口包含独立 PG、SQLite、并发、真实 Docker crash 等新增套件，可能继续暴露尚未实测的问题。

### C. 本地通过后提交并 push

核验工作区和用户已有提交，用精确 pathspec 分批暂存任务文件；完整依赖契约、迁移、测试和 Spec/Plan/Task 一并记录。审查 staged diff 后提交/push 当前正确分支。构建仓跨仓配置变更也需审查和妥善提交，不能遗落或夹带无关文件。

### D. 测试镜像与 Chart 发布

```sh
cd /Users/soddy/Documents/git-workspace/build-agent-docker
make setup k8s-helm-rcoder-version-publish ENV=test
```

已核对源码：
- `makefiles/01-init.mk:setup` 会更新多个工程；先核对真实 rcoder checkout/构建副本能取到刚 push 的提交。
- `makefiles/20-local-fast.mk` 默认重建 rcoder-k8s、agent-runner、app-runtime-base、app-runtime，并复制其他镜像，自动 bump patch 再发布 Chart。
- **不要用 `make -n` 预览该发布目标**：其递归 Make recipe 可能仍执行版本修改及镜像复制。
- 当前版本 0.1.265，若期间未变，预期下一 patch 是 0.1.266；但必须读取实际成功产物版本，不能提前硬编码。
- 发布失败先判断哪些阶段已完成，不能盲目再调用整条命令重复 bump。

### E. 部署个人测试集群，再跑 K8s E2E

SSH：`soddy@192.168.32.131`。先核对该机 kubectl context、节点、namespace、当前 release 和存储健康；仅目标测试环境。

用实际发布版本替换：
```sh
helm upgrade --install nuwax-k8s-test \
  oci://nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-k8s-test/nuwax-platform \
  --version <实际发布版本> -n nuwax-k8s-test \
  --reset-values --timeout 15m
```

观察 rollout、事件、镜像 digest、PG/userApp 后端和多副本。MDS 异常先诊断归属、Ceph/存储状态及依赖，不盲删 MDS/PVC、数据池或业务工作负载。rcoder K8s 日志通常在 Pod `/app/logs/` 文件，不以 `kubectl logs` 空输出当无日志。

现有入口：
```sh
make test-e2e-k8s-userapp \
  TEST_K8S_SSH=soddy@192.168.32.131 \
  RCODER_URL=http://192.168.32.131:<实际rcoder端口> \
  E2E_PINGORA_URL=http://192.168.32.131:<实际proxy端口>
```
实现：`tests-e2e/tools/k8s_userapp.py`（默认 namespace nuwax-k8s-test）。端口不能猜，先读取实际 Service。

**该脚本尚未针对本轮新生命周期协议实测/整体升级。** 现有覆盖首开并发、跨副本文件/取消、A/B 制品、cold/hot/env拒绝、stop/wake、清理和基线保留；需复核 lifecycle_id/request_id/operation_id、删除请求、恢复证据与多副本退出窗口是否完整。缺项先补契约测试与脚本，不直接将旧场景通过等同于新计划全通过。另有 `test-e2e-k8s`/k8s_lb 套件，先查其独占 namespace 和环境门控，勿误用到生产。

## 6. 交付要求

最终分别报告：组件、SQLite、真实 PG、本地 Docker、K8s API 契约、真实个人 K8s。

列出源码提交、构建版本、镜像身份、运行产物、报告路径、各命令 exit code、测试资源定向清理结果及真实未覆盖项。更新 Spec/Plan/Task，历史证据不要覆盖成当前通过。完整目标包括用户新增的测试环境发布部署，不要在本地组件绿灯后提前宣称完成。

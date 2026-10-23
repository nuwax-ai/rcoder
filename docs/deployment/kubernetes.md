# Kubernetes 形态

同一套代码的 K8s 运行形态：rcoder 作为控制面 Pod 管理动态创建的 agent 与 UserApp 资源。构建时启用 `kubernetes,rcoder-pg` feature（UserApp 控制面强制 PostgreSQL）。

## 核心资源模型

| 资源 | 形态 | 生命周期语义 |
|------|------|-------------|
| **Agent Runner** | StatefulSet + per-agent PVC | **停止不删 PVC**：数据保留，重建后挂回复用；容器 OOM 触发容器级重启自愈（Pod 不重建，IP 不变） |
| **UserApp prod** | Deployment + per-app PVC | 删除运行容器默认保留存储；连数据面销毁须显式 `purge`；应用级 scale-to-zero（停止/闲置回收），**流量到达自动唤醒** |
| **UserApp dev** | UserappBuilder（非常驻） | 空闲自动回收，PVC 保留复用 |

gateway 侧（rcoder-gateway）：无状态网关做 header 注入与路由，多副本共享 PG 控制面。

<a id="remote-workflow"></a>
## 远端 K8s 开发与验证

远端工作流从本地编辑工作树，经 Mutagen 同步到构建主机，再创建独立源码快照，在 Linux 构建 amd64 镜像并推送，按 digest 部署到专用测试 namespace。同步不是构建或部署；构建镜像也不代表执行过 Rust 组件测试。

### 环境准备

复用仓库根未跟踪的 `.env.local`，按 [配置模板](../../tools/remote_k8s/env.example) 补充 SSH、远端目录、context、namespace、registry、基础镜像和访问地址；不要覆盖已有无关项。构建主机与全部节点必须能拉取所选镜像。模型相关场景还需要模板列出的 LLM 配置。

仅选择自己获准使用的测试环境。所有环境值保留在本地配置，不写入公共示例；不要向共享生产环境运行测试。资源预算、超时和存储类以该环境实际配置为准。

```bash
make remote-k8s-doctor
make remote-k8s-sync-start
make remote-k8s-sync-status
make remote-k8s-verify SUITE=smoke

# 同一源码快照和部署追加受影响套件。
make remote-k8s-test SUITE=userapp
make remote-k8s-test SUITE=chat
make remote-k8s-test SUITE=gateway

# 失败后收集诊断。
make remote-k8s-logs
```

`verify` 校验并构建当前源码、部署后测试；`test` 只验证已部署版本。本地源码修改后必须重新 `verify`。同一环境构建、部署、测试串行，测试期间保持源码与部署稳定。远端套件使用 `SUITE`，不要与 Compose 的 `E2E_SUITE`/`E2E_FILTER` 混用。

### 套件与证据

需要生产工作区根的 Linux 精确回归可单独执行 `validation` 构建目标。它从同一冻结源码运行正式 HTTP Restart 用例，`/app` 使用临时内存挂载，复用该专用 builder 的 SDK/Cargo 缓存；成功必须实际执行且恰好通过一个测试。Mac 未执行该 Linux 用例不能记为通过。此目标不发布镜像，也不是普通三个角色镜像的构建依赖。

```bash
# 在获准的远端构建主机执行；变量来自本轮冻结源码与 builder 回执。
docker buildx build --builder "$OWNED_BUILDER" --platform linux/amd64 \
  --file "$FROZEN_SOURCE/docker/remote-k8s/Dockerfile" \
  --target validation --output type=cacheonly --progress plain \
  --build-arg "RUST_IMAGE=$RUST_IMAGE_DIGEST" --build-arg CARGO_JOBS=2 \
  --build-arg "RCODER_BASE=$RCODER_BASE_DIGEST" \
  --build-arg "COMPUTER_BASE=$COMPUTER_BASE_DIGEST" \
  --build-arg "RUNTIME_BASE=$RUNTIME_BASE_DIGEST" \
  "$FROZEN_SOURCE"
```

| 套件 | 验证范围 |
|---|---|
| `smoke` | 当前部署身份、双副本就绪、PVC Bound 和直连健康接口 |
| `userapp` | UserApp 工作区、构建与部署；检查完整场景报告 |
| `chat` | 真实 AI、会话与续传；缺模型配置不能视为通过 |
| `gateway` | Gateway/HTTPRoute 条件与实际 HTTP 路径 |
| `orphan` | 独立 `PROFILE=orphan`：双 Main + PG 下，验证 Service-only/停止的空控制器自动回收、运行或 Pending 实例及 canonical/lifecycle 引用保护、动态 CephFS/RBD PVC 与数据保留；不调用模型 |
| `all` | 组合验证；完整范围以启动器场景目录为准 |

直连与 Gateway 是不同路径。实际地址从本地部署回执的 `url`、`gateway_url` 读取，IP + NodePort 可作为直连入口。Gateway 不可用时可以运行不依赖它的直连场景，但不能替换路径后宣称 Gateway 通过。smoke 不证明完整生命周期、跨副本业务或动态应用路由正确。

源码摘要、镜像 digest、namespace、部署与测试回执写入本地 `.remote-k8s/`；E2E 明细写入 `tests-e2e/reports/`，均不提交。实际问题与每轮结果另记入本地工作文档，不将历史报告当作当前通过。严格验收规则见 [E2E 说明](../../tests-e2e/tools/README.md)。

`orphan` 是独立完整套件，不是 `chat` 的筛选项，也不纳入默认 `all`。在获准的专用 `rcoder-e2e-*` namespace，将本地环境配置设为 `REMOTE_K8S_PROFILE=orphan` 后执行 `make remote-k8s-verify SUITE=orphan`；同一部署追加可用 `make remote-k8s-test SUITE=orphan`。该 fixture 不挂载 CephFS 根，只创建自身动态 PVC；保留基础及本轮数据卷。为构造真实 replicas=1/no-Pod 窗口，套件会短暂限制该专用 namespace 的 Pod 数，不影响已有双 Main/PG。部分畸形登记通过本 fixture 的 PostgreSQL 定向注入，模拟不能 hydrate 的历史数据；不修改共享数据库。报告逐项记录 required assertions、未执行项、原失败、精确 UID 清理及保留卷，工具组件测试不代表该真实集群套件通过。

### 诊断与资源保护

先运行 `make remote-k8s-logs`，结合 RCoder 容器内 `/app/logs/`、agent_runner stdout、Pod 状态与事件定位问题。`doctor` 仅检查前置，不证明网络和存储全链路可用。

只操作本环境拥有的资源、同步会话和镜像标签。agent PVC 永不删除，停止计算保留 PVC；`remote-k8s-down` 保留 namespace 和数据卷。namespace 不能作为共享 CephFS 数据隔离依据，禁止删除共享存储根数据。

## 本地 K8s 开发（devspace）

```bash
make devspace-init   # 初始化（namespace: rcoder-dev）
make devspace-dev    # 启动
curl http://127.0.0.1:8290/health
```

本地集群可使用 OrbStack 等；devspace 负责源码同步，改码后需重启容器内 rcoder 进程生效（`/app/dev-rcoder.sh restart`）。

> ⚠️ K8s 下 rcoder 日志**写文件不写 stdout**（`/app/logs/rcoder.YYYY-MM-DD`，JSON 按天滚动）——`kubectl logs` 几乎为空属正常现象，排障要看容器内文件日志。

## 生产部署

生产的 Helm chart 与部署清单由**独立的部署仓库**维护（不在本仓库内）；本仓库提供 `k8s/config`（配置样例）与 `k8s/scripts`（辅助脚本）。部署时的关键决策：

- 存储类与 PVC 容量按集群实际后端配置（代码默认值会被部署配置覆盖）
- gRPC 寻址：`{pod}-svc.{ns}.svc.cluster.local:50051`
- 网络：本地集群用 Envoy Gateway，生产可用 Cilium 等网关——rcoder 控制面代码与网关选型无关

## 相关形态

- [Docker Compose 形态](docker.md)：本地开发推荐
- [宿主机形态（deploy-host）](host.md)：本地 Docker/K8s 管动态容器的单机形态（含 K8s 形态三前置）
- [UserApp 应用管理](../concepts/userapp.md)：K8s 下的 UserApp 生命周期语义

### UserApp 保留存储恢复

身份或计算资源缺失时的默认兜底、开关和验收边界见 [UserApp 保留存储恢复](../userapp-default-recovery.md)。

# 宿主机单机形态（deploy-host）

rcoder 的第三种部署形态：**控制平面直接跑在宿主机**，用本地 Docker（必需）或本地 K8s（可选，经 kubeconfig）运行动态创建的 agent/UserApp 容器——"有 Docker 就能跑"的单机形态，定位为本地开发与未来客户端产品（desktop）的运行基座。与容器/K8s 部署形态互补（后者保多租户/多副本/团队场景）。

分层结构：引擎在 `rcoder-engine`、HTTP 面在 `http-server`、组合根 `rcoder`（lib `run()` + 薄 bin）；desktop（gpui-kit 客户端）复用 `rcoder_engine::assemble` 同一装配序进程内跑完整服务。

## 快速开始

```bash
# Docker 宿主机形态（OrbStack / Docker Desktop）
make dev-host

# K8s 宿主机形态（本地 kubeconfig，如 OrbStack k8s）
make dev-host-k8s
```

等价的原始命令：

```bash
cargo run -p rcoder --bin rcoder --features deploy-host
CONTAINER_RUNTIME=kubernetes cargo run -p rcoder --bin rcoder --features kubernetes,deploy-host
```

## 默认目录约定（`~/.rcoder`）

| 容器根（常量锚点） | 宿主机默认 | 用途 |
| --- | --- | --- |
| `/app/project_workspace` | `~/.rcoder/workspace/projects` | web agent 工作区 |
| `/app/computer-project-workspace` | `~/.rcoder/workspace/computer` | computer agent 工作区 |
| `/app/userapp-workspace` | `~/.rcoder/workspace/userapp` | UserApp dev/prod 树 |
| `/app/data` | `~/.rcoder/data` | 本地存储 |
| `/app/logs` | `~/.rcoder/logs` | 日志 |
| `/app/agent-cache` | `~/.rcoder/agent-cache` | agent 下载缓存 |

- config 首启自动生成 `~/.rcoder/config.yml`（缺省即建，含随机 api key）。
- 本地 `cargo run` 开发想复用仓库目录：`RCODER_CONFIG_FILE=config.yml` + `RCODER_DEPLOY_HOST_PATH_MAP`（见下）把各根覆盖为仓库相对路径（`make/dev-host.mk` 注释里有完整组合示例）。

## 环境变量清单

| env | 默认（deploy-host） | 说明 |
| --- | --- | --- |
| `RCODER_CONFIG_FILE` | `~/.rcoder/config.yml` | config 文件位置（双形态都优先生效） |
| `RCODER_DEPLOY_HOST_PATH_MAP` | 空 | 容器根→宿主根增量覆盖，`/app/x=/abs/path,...` |
| `RCODER_DEPLOY_HOST_NETWORK` | `rcoder-agent-network` | agent 容器网络（不存在自动创建） |
| `RCODER_DEPLOY_HOST_REACH` | `auto` | 容器寻址模式：`direct`（容器 IP 直拨零发布）/ `published`（发布到宿主机）/ `auto`（按 socket 检测）；优先于 config `deploy_host.reach` |
| `RCODER_BIND_HOST` | `127.0.0.1`（容器形态 `0.0.0.0`） | 主 HTTP 端口 bind 地址 |
| `RCODER_PORT` | config 的 `port` | 与本地 Compose 同时运行时为宿主机实例指定独立 HTTP 端口 |
| `DOCKER_SOCKET_PATH` | `/var/run/docker.sock` | OrbStack 备选 `$HOME/.orbstack/run/docker.sock` |
| `RCODER_K8S_NAMESPACE` | `default` | K8s 形态 namespace |
| `RCODER_K8S_STORAGE_CLASS` | `local-path`（容器形态 `rcoder-nfs`） | PVC storage class |
| `RCODER_K8S_PVC_ACCESS_MODE` | `ReadWriteOnce`（容器形态 `ReadWriteMany`） | PVC 访问模式 |
| `RCODER_AGENT_RUNNER_SA` | `rcoder-pods-sa` | agent Pod ServiceAccount |

## 端口行为

- **主端口**（config `port`）：全部 HTTP 面（含默认集成的 file-server 路由），deploy-host 默认只监听 `127.0.0.1`。
- **60000**：file-server-proxy dispatcher 照常保留；deploy-host 下 Rust 上游 loopback 对准本进程主端口，不配 TS 60001 上游（all_rust 策略）——主端口与 60000 双入口并存、路由行为一致。
- **Pingora 8088**：数据面（VNC/ttyd/dbx/预览）照常。
- **agent 容器端口**（8086/50051/6080/17681/60000/4224，builder 加 3010/9080/6091）：按 **Reach 模式**（env `RCODER_DEPLOY_HOST_REACH` > config `deploy_host.reach` > `auto` 检测）二选一：

| Reach | 行为 | 适用环境 |
| --- | --- | --- |
| `direct` | **零端口发布**，注册表登记容器真实 IPv4，拨 `{ip}:{容器端口原值}` | OrbStack、原生 Linux dockerd（容器网段从宿主机可路由） |
| `published` | 端口发布到宿主机全部网卡（Docker 自动分配 host port），拨 `127.0.0.1:{host_port}` | Docker Desktop（容器网段对宿主机不可路由，发布端口是唯一可达路径） |
| `auto`（默认） | 按生效 Docker socket 特征检测：`.orbstack`→direct；`com.docker.docker`/`docker.raw`→published；Linux 原生→direct；未知→published（安全默认） | — |

  **OrbStack 用户注意**：`auto` 在 OrbStack 下默认即 direct（行为相对 Published 时代翻转）——需要发布端口给内网其他机器访问时，显式设 `RCODER_DEPLOY_HOST_REACH=published` 一行回退。K8s 形态不受 Reach 影响（Service NodePort 路径不咨询模式）。Direct 模式绝不使用 `容器名.orb.local`（fake-IP 恒超时）。模式进程级一次，容器创建前决定（Docker 创建后不能补绑端口）；`tunnel`（R2 yamux/R3 iroh）为契约占位，配置可写但启动显式拒绝。

## 安全明示

- docker.sock 等价 root：与容器内 rcoder 挂载 docker.sock 同等权限面，非新增风险，但宿主机形态感知更直接——默认只监听 `127.0.0.1` + api_key_auth 首启启用，不要在不受信网络上开放 `RCODER_BIND_HOST`。
- 单租户单实例定位：多实例共写 `~/.rcoder` 属误用；desktop 与同机 rcoder server 占同一组端口，二选一。

## 快速集成回归

`tests-e2e` 复用一条不依赖 LLM 的 UserApp dev 业务流程：创建工作区、Stop、等待操作终态、Restart、核验原工作区数据和生命周期。运行环境分别核验 Docker 挂载与端口、K8s Pod 与 PVC 身份。Compose 的完整业务回归和远端 K8s 的 Helm/RBAC/多节点验收仍使用原入口。

| RCoder 运行形态 | 先启动 | 聚焦 UserApp 计算场景 |
| --- | --- | --- |
| Docker Compose | `make dev-up` | `E2E_SUITE=compose_userapp E2E_FILTER=userapp_dev_compute_shared_contract make test-e2e-compose` |
| macOS/Linux 宿主机 + Docker Published | `RCODER_PORT=<独立端口> make dev-host-published` | `RCODER_URL=http://127.0.0.1:<独立端口> make test-e2e-host-userapp` |
| 宿主机 + 本地 K8s | `make dev-host-k8s`（按下述隔离环境配置） | `make test-e2e-host-k8s-userapp` |

宿主机实例与 Compose 并行时，还须为 Pingora 和 file-server-proxy 分配独立监听端口；用独立 `RCODER_CONFIG_FILE` 与工作区映射，不共写当前 Compose 数据。宿主机 Docker 的 `host` 组要求 Published；OrbStack 的 `auto` 默认 Direct，应使用 `make dev-host-published`。原 `make test-e2e-host-direct` 检查 Direct 的普通 agent 寻址。

本地 K8s 严格启动器要求显式导出 `KUBECONFIG`、本机 `RCODER_URL`、`TEST_K8S_NS` 与相同的 `RCODER_K8S_NAMESPACE`。namespace 必须是独立的 `rcoder-*`，测试期间所有 `kubectl` 均显式指定该 namespace。先按已有宿主机 K8s 前置准备 PG、resource limits 与工作区 PVC。UserApp 场景用专属 app ID；正常结束通过 RCoder 删除其测试数据，失败且结果未知时保留现场并报告残留，不直接删除 PVC。

OrbStack 首次供卷和拉起 UserApp Pod 可能超过默认 90 秒。运行本地 K8s UserApp 场景前，可在启动宿主机 RCoder 时设 `RCODER_USERAPP_ENSURE_TIMEOUT_SECONDS=240`；测试 HTTP 预算为 300 秒。一次超时不代表后台创建已结束，应先查询该测试应用的生命周期与实际资源，再通过 RCoder 的删除入口收尾。

## K8s 形态三前置（`make dev-host-k8s`）

K8s 形态（本地 kubeconfig 直连 OrbStack k3s 等）比 Docker 形态多三个前置，缺一会在启动或首个 agent 创建时 fail-fast：

| 前置 | 原因（既有约束） | 满足方式 |
| --- | --- | --- |
| userApp 控制面 PostgreSQL | K8s access mode 强制 PG（`resolved_backend`，多副本共享控制面设计） | 本地容器 `docker run -d --name rcoder-host-pg -e POSTGRES_PASSWORD=... -p 127.0.0.1:55432:5432 postgres:17` + env `RCODER_USERAPP_STORAGE_BACKEND=postgres` `RCODER_USERAPP_PG_URL=postgres://...@127.0.0.1:55432/userapp` |
| `kubernetes_config.services` 配 `resource_limits` | K8s 模式下 fail-fast 拒绝降级 docker_config（helpers.rs `resolve_resource_limits_from_config`） | `~/.rcoder/config.yml` 的 `kubernetes_config.services.{service}` 段补 `resource_limits` 与本地镜像 |
| 共享 workspace PVC 预建 | 运行时假定部署链已建 | `kubectl apply` `{ns}-rcoder-computer-workspace`（10Gi）——**单节点 local-path/RWO 即可**（OrbStack 单节点下 RWO 与 RWX 行为等价；多节点才需 CephFS/NFS 的 RWX） |

agent 镜像：OrbStack 的 Docker 与 K8s **共享镜像仓库**——本地 `docker build -t dev-rcoder-agent-runner:latest` 后 Pod `IfNotPresent` 直接命中。

## 已知限制

- agent 容器 egress 在独立 bridge（`rcoder-agent-network`），无法解析 compose 环境服务名（如内网 LLM 网关）——LLM `base_url` 请配 IP/localhost。
- K8s NodePort 段 30000-32767（共 2768 个），每 agent 6-9 个端口——本地开发可接受，长期多 agent 需固定段分配（roadmap）。
- Direct 模式依赖容器网段从宿主机可路由——Docker Desktop（macOS）虚拟网络不可达，`auto` 检测会选 published；手动强制 direct 而 socket 特征未知时运行期连接失败可归因（注册表未登记→拨号回退 warn 日志）。
- Published 模式下，builder 重启重建会重新发布所需端口并核对绑定；可用 `make test-e2e-host-userapp` 检查这一行为。Direct 模式在替换容器启动并取得新 IP 后更新登记。
- app-cli 保持独立命令（npm `@nuwax-ai/app-cli` 自装）；宿主机形态 UserApp 编排仍在容器内，宿主机进程不需要 app-cli。

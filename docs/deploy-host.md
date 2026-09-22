# deploy-host 宿主机运行形态

rcoder 的第二种部署形态：**控制平面直接跑在宿主机**，用本地 Docker（必需）
或本地 K8s（可选，经 kubeconfig）运行动态创建的 agent/UserApp 容器——
"有 Docker 就能跑"的单机形态，定位为本地开发与未来客户端产品（desktop）
的运行基座。与容器/K8s 部署形态互补（后者保多租户/多副本/团队场景）。

分层结构：引擎在 `rcoder-engine`、HTTP 面在 `http-server`、组合根 `rcoder`
（lib `run()` + 薄 bin）；desktop（gpui-kit 客户端）复用 `rcoder_engine::assemble`
同一装配序进程内跑完整服务。

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
| `/app/data` | `~/.rcoder/data` | Turso 等存储 |
| `/app/logs` | `~/.rcoder/logs` | 日志 |
| `/app/agent-cache` | `~/.rcoder/agent-cache` | agent 下载缓存 |

- config 首启自动生成 `~/.rcoder/config.yml`（缺省即建，含随机 api key）。
- 本地 `cargo run` 开发想复用仓库目录：`RCODER_CONFIG_FILE=config.yml` +
  `RCODER_DEPLOY_HOST_PATH_MAP`（见下）把各根覆盖为仓库相对路径
  （`make/dev-host.mk` 注释里有完整组合示例）。

## 环境变量清单

| env | 默认（deploy-host） | 说明 |
| --- | --- | --- |
| `RCODER_CONFIG_FILE` | `~/.rcoder/config.yml` | config 文件位置（双形态都优先生效） |
| `RCODER_DEPLOY_HOST_PATH_MAP` | 空 | 容器根→宿主根增量覆盖，`/app/x=/abs/path,...` |
| `RCODER_DEPLOY_HOST_NETWORK` | `rcoder-agent-network` | agent 容器网络（不存在自动创建） |
| `RCODER_BIND_HOST` | `127.0.0.1`（容器形态 `0.0.0.0`） | 主 HTTP 端口 bind 地址 |
| `DOCKER_SOCKET_PATH` | `/var/run/docker.sock` | OrbStack 备选 `$HOME/.orbstack/run/docker.sock` |
| `RCODER_K8S_NAMESPACE` | `default` | K8s 形态 namespace |
| `RCODER_K8S_STORAGE_CLASS` | `local-path`（容器形态 `rcoder-nfs`） | PVC storage class |
| `RCODER_K8S_PVC_ACCESS_MODE` | `ReadWriteOnce`（容器形态 `ReadWriteMany`） | PVC 访问模式 |
| `RCODER_AGENT_RUNNER_SA` | `rcoder-pods-sa` | agent Pod ServiceAccount |

## 端口行为

- **主端口**（config `port`）：全部 HTTP 面（含默认集成的 file-server 路由），
  deploy-host 默认只监听 `127.0.0.1`。
- **60000**：file-server-proxy dispatcher 照常保留（Java 对接零调整）；
  deploy-host 下 Rust 上游 loopback 对准本进程主端口，不配 TS 60001 上游
  （all_rust 策略）——主端口与 60000 双入口并存、路由行为一致。
- **Pingora 8088**：数据面（VNC/ttyd/dbx/预览）照常。
- **agent 容器端口**（8086/50051/6080/17681/60000/4224，builder 加
  3010/9080/6091）：Docker 形态发布到宿主机（Docker 自动分配）；K8s 形态
  agent Service 自动 NodePort 化。rcoder 拨号一律经 published-port 注册表
  解析 `127.0.0.1:{host_port}`——macOS 宿主机无法路由容器网段 IP，也无法
  解析集群内 FQDN，发布端口是唯一可达路径。

## 安全明示

- docker.sock 等价 root：与容器内 rcoder 挂载 docker.sock 同等权限面，非新增
  风险，但宿主机形态感知更直接——默认只监听 `127.0.0.1` + api_key_auth 首启
  启用，不要在不受信网络上开放 `RCODER_BIND_HOST`。
- 单租户单实例定位：多实例共写 `~/.rcoder` 属误用；desktop 与同机 rcoder
  server 占同一组端口，二选一。

## K8s 形态三前置（`make dev-host-k8s`）

K8s 形态（本地 kubeconfig 直连 OrbStack k3s 等）比 Docker 形态多三个前置，
缺一会在启动或首个 agent 创建时 fail-fast：

| 前置 | 原因（既有约束） | 满足方式 |
| --- | --- | --- |
| userApp 控制面 PostgreSQL | K8s access mode 强制 PG（`resolved_backend`，多副本共享控制面设计） | 本地容器 `docker run -d --name rcoder-host-pg -e POSTGRES_PASSWORD=... -p 127.0.0.1:55432:5432 postgres:17` + env `RCODER_USERAPP_STORAGE_BACKEND=postgres` `RCODER_USERAPP_PG_URL=postgres://...@127.0.0.1:55432/userapp` |
| `kubernetes_config.services` 配 `resource_limits` | K8s 模式下 fail-fast 拒绝降级 docker_config（helpers.rs `resolve_resource_limits_from_config`） | `~/.rcoder/config.yml` 的 `kubernetes_config.services.{service}` 段补 `resource_limits` 与本地镜像 |
| 共享 workspace PVC 预建 | 运行时假定部署链已建（devspace/remote-k8s 均有对应清单） | `kubectl apply` `{ns}-rcoder-computer-workspace`（10Gi）——**单节点 local-path/RWO 即可**（OrbStack 单节点下 RWO 与 RWX 行为等价；多节点才需 CephFS/NFS 的 RWX） |

agent 镜像：OrbStack 的 Docker 与 K8s **共享镜像仓库**——本地
`docker build -t dev-rcoder-agent-runner:latest` 后 Pod `IfNotPresent` 直接命中。

## 已知限制

- agent 容器 egress 在独立 bridge（`rcoder-agent-network`），无法解析 compose
  环境服务名（如内网 LLM 网关）——LLM `base_url` 请配 IP/localhost。
- K8s NodePort 段 30000-32767（共 2768 个），每 agent 6-9 个端口——本地
  开发可接受，长期多 agent 需固定段分配（roadmap）。
- app-cli 保持独立命令（npm `@nuwax-ai/app-cli` 自装）；宿主机形态 UserApp
  编排仍在容器内，宿主机进程不需要 app-cli。

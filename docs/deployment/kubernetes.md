# Kubernetes 形态

同一套代码的 K8s 运行形态：rcoder 作为控制面 Pod 管理动态创建的 agent 与 UserApp 资源。构建时启用 `kubernetes,rcoder-pg` feature（UserApp 控制面强制 PostgreSQL）。

## 核心资源模型

| 资源 | 形态 | 生命周期语义 |
|------|------|-------------|
| **Agent Runner** | StatefulSet + per-agent PVC | **停止不删 PVC**：数据保留，重建后挂回复用；容器 OOM 触发容器级重启自愈（Pod 不重建，IP 不变） |
| **UserApp prod** | Deployment + per-app PVC | 删除运行容器默认保留存储；连数据面销毁须显式 `purge`；应用级 scale-to-zero（停止/闲置回收），**流量到达自动唤醒** |
| **UserApp dev** | UserappBuilder（非常驻） | 空闲自动回收，PVC 保留复用 |

gateway 侧（rcoder-gateway）：无状态网关做 header 注入与路由，多副本共享 PG 控制面。

## 本地 K8s 开发（devspace）

```bash
make devspace-init   # 初始化（namespace: rcoder-dev）
make devspace-dev    # 启动
curl http://127.0.0.1:8290/health
```

本地集群推荐 OrbStack 等；devspace 负责源码同步，改码后需重启容器内 rcoder 进程生效（`/app/dev-rcoder.sh restart`，增量编译约 1 分钟）。

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

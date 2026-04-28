# RCoder K8s 部署方案 —— 架构详解

> 本文档详细说明 `rcoder` 项目在 Kubernetes 上的完整部署架构。
> 代码仓库: `/home/swufe/gitworkspace/rcoder/k8s/`

---

## 1. 整体架构

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                           Kubernetes 集群                                    │
│                                                                              │
│  ┌──────────────────────────────────────────────────────────────────────┐   │
│  │                      Ingress Controller (可选)                          │   │
│  │                    className 可配置 (nginx/traefik/alb)                 │   │
│  └──────────────────────────────────────────────────────────────────────┘   │
│                                    │                                        │
│                                    ▼                                        │
│  ┌──────────────────────────────────────────────────────────────────────┐   │
│  │                     rcoder Service (NodePort 30080)                   │   │
│  │                   Deployment / Pod (rcoder 主服务)                     │   │
│  │                                                                       │   │
│  │   • REST API :8087 (health / chat / agent 管理)                      │   │
│  │   • Pingora  :8088 (内部反向代理,给动态 agent-runner Pod 用)           │   │
│  │                                                                       │   │
│  │   动态创建 Agent Runner Pod ←── K8s API (不挂 docker.sock)           │   │
│  └──────────────────────────────┬───────────────────────────────────────┘   │
│                                 │                                            │
│                                 ▼                                            │
│  ┌──────────────────────────────────────────────────────────────────────┐   │
│  │              JuiceFS StorageClass (RWX, 跨节点共享)                     │   │
│  │                     juicefs-sc-{release}                              │   │
│  │                                                                       │   │
│  │   PVC: rcoder-workspace (50Gi)                                       │   │
│  │     ├── /app/project_workspace        ← rcoder 主服务读写              │   │
│  │     └── /app/computer-project-workspace  ← computer agent runner 用    │   │
│  └───────────────────────────────┬──────────────────────────────────────┘   │
└──────────────────────────────────┼──────────────────────────────────────────┘
                                   │
               ┌───────────────────┼────────────────────┐
               │                   │                    │
               ▼                   ▼                    ▼
┌──────────────────────────┐ ┌────────────────┐ ┌─────────────────────────────┐
│      PostgreSQL          │ │     MinIO      │ │   JuiceFS CSI Driver         │
│   StatefulSet (RWO)      │ │  StatefulSet   │ │     (kube-system)            │
│                          │ │    (RWO)       │ │                             │
│  • JuiceFS 元数据存储     │ │                │ │  • csi-provisioner           │
│  • DB: juicefs           │ │  bucket: juicefs│ │  • csi-node-driver-registrar │
│  • Longhorn / local-path │ │                │ │  • juicefs-plugin (DaemonSet) │
│  • 10Gi PVC             │ │  Longhorn /     │ │                             │
│                          │ │  local-path     │ │                             │
└──────────────────────────┘ │  30Gi PVC      │ └─────────────────────────────┘
                              └────────────────┘
               ▲                                           │
               │                                           │
               └───────────────────────┬─────────────────┘
                                       │
                              ┌────────▼────────┐
                              │   Longhorn /    │
                              │   local-path    │
                              │   (块存储 RWO)  │
                              └─────────────────┘
```

---

## 2. 存储架构详解

### 2.1 存储分层

| 层次 | 存储类型 | 访问模式 | 使用场景 | 典型大小 |
|------|---------|---------|---------|---------|
| **共享文件系统** | JuiceFS (CSI) | RWX 跨节点 | rcoder workspace, project_workspace, computer_workspace | 50Gi |
| **对象存储** | MinIO (S3) | RWO 单节点 | JuiceFS 数据块后端 + 应用文件存储 | 30Gi+ |
| **元数据库** | PostgreSQL | RWO 单节点 | JuiceFS 文件元数据 (inode/权限/目录结构) | 10Gi |
| **块存储** | Longhorn / local-path | RWO 单节点 | PostgreSQL 数据盘, MinIO 数据盘 | 按需 |

### 2.2 JuiceFS 数据流

```
用户/应用 Pod  ──写入──▶  JuiceFS FUSE Mount (CSI Driver)
                                    │
                                    ▼
                          PostgreSQL (元数据)
                          • 文件名 / inode 编号
                          • 权限 / 所有者
                          • 目录结构
                          • 硬链接 / 符号链接
                                    │
                                    ▼
                          MinIO S3 (数据块)
                          • chunk-xxx 文件
                          • juicefs.db 内部索引
```

**为什么这样分层：**
- **JuiceFS = 共享层**：提供 POSIX RWX，多节点 rcoder Pod 和动态创建的 agent-runner Pod 可以同时读写同一份项目文件
- **PostgreSQL = 元数据**：文件系统"目录结构/文件名/权限"存这里，JuiceFS mount pod 重启不丢文件索引
- **MinIO = 数据块后端**：文件内容 chunk 存在 MinIO bucket，不存在节点本地，JuiceFS mount pod 重启不丢数据
- **Longhorn/local-path = 数据库盘**：MySQL/Redis/Milvus/ES 不能用 JuiceFS（延迟太高），强制走块存储

### 2.3 Longhorn vs local-path

| 维度 | local-path (k3s 内置) | Longhorn |
|------|----------------------|---------|
| 多节点副本 | ❌ 单节点独占 | ✅ 3 副本同步分布不同节点 |
| 扩容 | ❌ 手动迁数据 | ✅ UI/CLI 秒扩 |
| 快照/回滚 | ❌ | ✅ |
| 备份到 S3 | ❌ | ✅ |
| UI 管理 | ❌ | ✅ (:9000) |
| 资源开销 | ~0 | ~500MB agent/node |
| 适用场景 | 单节点 dev | 多节点 prod |

**扩展策略**：
- 当前 `values.yaml` 里 `storageClass: local-path`（dev够用）
- 未来多节点集群装好 Longhorn 后，只需把 `storageClass` 改为 `longhorn`，所有 StatefulSet 的 PVC 自动用 Longhorn

### 2.4 JuiceFS Secret 关键配置

```yaml
# juicefs-secret (渲染后)
name:        "rcoder-juicefs"
metaurl:     "postgres://juicefs:<pass>@postgresql.<ns>:5432/juicefs"
storage:     "minio"
bucket:      "http://minio-service.<ns>:9000/juicefs"
access-key:  "<minio-root-user>"
secret-key:  "<minio-root-password>"
```

---

## 3. 核心组件

### 3.1 rcoder 主服务 (Deployment)

**关键环境变量**：

| 环境变量 | 值 | 作用 |
|---------|---|------|
| `CONTAINER_RUNTIME` | `kubernetes` | 激活 K8s API 模式，创建动态 Pod 而非 Docker 容器 |
| `RCODER_K8S_NAMESPACE` | `{{ .Release.Namespace }}` | rcoder 通过此 namespace 创建 agent-runner Pod |
| `RCODER_K8S_STORAGE_CLASS` | `juicefs-sc-{{ .Release.Name }}` | 动态 Pod 挂 JuiceFS PVC 用的 SC 名 |
| `RCODER_DOCKER_IMAGE` | `rcoder-k8s:latest` | agent-runner Pod 的基础镜像 |
| `RCODER_DOCKER_IMAGE_COMPUTER` | `rcoder-computer-agent-runner:latest` | computer agent runner 镜像 |

**挂载的卷**：

| 卷名 | 来源 | 挂载路径 | 用途 |
|------|------|---------|------|
| rcoder-config | ConfigMap | `/app/config.yml` | rcoder 运行时配置 |
| rcoder-workspace | JuiceFS PVC (RWX) | `/app/project_workspace` (subPath: `project_workspace`) | 项目工作区 |
| rcoder-workspace | JuiceFS PVC (RWX) | `/app/computer-project-workspace` (subPath: `computer_workspace`) | Computer agent 工作区 |

**权限模型**：
- ServiceAccount: `rcoder-pods-sa`
- ClusterRole: `rcoder-pods-clusterrole` (集群级，多 release 共享)
  - `pods`: create / delete / get / list / watch / patch / update
  - `pods/log`: get / list
  - `pods/exec`: create
  - `pods/status`: get
  - `persistentvolumeclaims`: get / list / watch / create / delete

### 3.2 动态 Agent Runner Pod

rcoder 主服务通过 K8s API 动态创建的临时 Pod：

```
rcoder 主服务 (Deployment)
    │
    ├── 收到 /chat 请求
    │
    ├── 调用 K8s API 创建 Pod:
    │     • name: rcoder-agent-runner-<session-id>
    │     • image: rcoder-computer-agent-runner
    │     • env: RCODER_K8S_NAMESPACE / RCODER_DOCKER_IMAGE_* 等
    │     • volumes:
    │           - project_workspace (JuiceFS RWX, subPath)
    │           - computer_workspace (JuiceFS RWX, subPath)
    │           - emptyDir cache (可重建)
    │
    └── Pod 内 gRPC 服务，rcoder 主服务通过 serviceDNS:50051 连接
```

**与 docker-compose 模式的区别**：

| 维度 | docker-compose (Docker) | K8s |
|------|------------------------|-----|
| 容器创建 | docker CLI / Docker API | K8s API |
| 通信方式 | Docker 内部网络 + 端口映射 | K8s ClusterIP Service DNS |
| 工作空间 | hostPath bind-mount | JuiceFS RWX PVC |
| 清理 | docker stop + rm | K8s Pod delete |
| socket | /var/run/docker.sock | 不需要 |

### 3.3 PostgreSQL (StatefulSet)

- **用途**：JuiceFS 元数据存储
- **数据库**：`juicefs` (JuiceFS 自动建)
- **用户**：`juicefs` (来自 credentials Secret)
- **存储**：Longhorn / local-path 块存储 PVC (RWO)
- **initContainers**: 清理 `lost+found` 目录避免 PG 初始化失败

### 3.4 MinIO (StatefulSet)

- **用途**：JuiceFS S3 数据块后端
- **bucket**：`juicefs` (由 minio-init Job 创建)
- **凭据**：来自 `rcoder-credentials` Secret (`MINIO_ROOT_USER` / `MINIO_ROOT_PASSWORD`)
- **存储**：Longhorn / local-path 块存储 PVC (RWO)
- **健康检查**：
  - Liveness: `/minio/health/live`
  - Readiness: `/minio/health/ready`

### 3.5 minio-init Job

- **触发时机**：`helm.sh/hook: post-install,post-upgrade`
- **作用**：确保 `juicefs` bucket 存在
- **策略**：`helm.sh/hook-delete-policy: before-hook-creation` 每次 upgrade 重新运行
- **镜像**：`minio/mc` (MinIO Client)

---

## 4. 部署方式

### 4.1 三种部署方式对比

| 方式 | 入口 | 适用场景 | 维护者 |
|------|------|---------|--------|
| **Kustomize** | `deploy-dev.sh` / `deploy-prod.sh` | 日常开发 / 在线环境快速迭代 | 开发团队 |
| **Helm** | `helm install ... k8s/helm/rcoder` | 参数化部署 / 对外交付 | 交付/运维 |
| **Offline Bundle** | `make k8s-offline-bundle` → `install.sh` | 政企客户完全断网 | 交付 |

### 4.2 Kustomize 部署流程

```
deploy-dev.sh 顺序执行:

[1/6] 检查 K8s 集群 + kubectl
        │
[1.5/6] 检查/安装 open-iscsi (Longhorn 依赖)
        │
[2/6] 部署 Longhorn (若未安装)
        │   └── kubectl apply -f https://raw.githubusercontent.com/longhorn/longhorn/master/deploy/longhorn.yaml
        │
[3/6] 部署 JuiceFS CSI Driver (若未安装)
        │   └── helm install juicefs-csi-driver juicedata/juicefs-csi-driver --namespace kube-system
        │
[4/6] Kustomize 部署应用 (manifests/overlays/dev)
        │   └── kubectl apply -k manifests/overlays/dev
        │         顺序:
        │         1. namespace.yaml
        │         2. storage/ (postgresql → minio → minio-init → juicefs-secret → juicefs-sc → juicefs-pvc)
        │         3. rcoder/ (SA → ClusterRole/CRB → ConfigMap → Deployment → Service → NetworkPolicy → PDB)
        │
[5/6] 验证部署状态
        │
[6/6] 输出访问信息
```

### 4.3 Helm 部署

```bash
# dev
helm install rcoder-dev k8s/helm/rcoder \
    --namespace nuwax-rcoder-dev --create-namespace \
    -f k8s/helm/rcoder/values-dev.yaml

# prod (密码通过 --set 注入)
helm install rcoder-prod k8s/helm/rcoder \
    --namespace nuwax-rcoder-prod --create-namespace \
    -f k8s/helm/rcoder/values-prod.yaml \
    --set credentials.postgresql.password=<real> \
    --set credentials.minio.rootPassword=<real>

# 同集群 Kustomize + Helm 并存 (Helm 接管 ClusterRole)
helm install rcoder-helm k8s/helm/rcoder \
    --set rcoder.clusterRole.create=false
```

### 4.4 离线部署

**构建 (有网机器)**：
```bash
make k8s-offline-bundle
# 产出: dist/rcoder-offline-<ver>-<arch>.tar.gz (~1.7-2GB, 27个镜像)
```

**安装 (客户内网, 两种模式)**：

| 模式 | 镜像导入方式 | 适用场景 |
|------|------------|---------|
| `--mode=direct` | `ctr image import` 到节点 containerd | 单节点 / 小集群, 无私有 registry |
| `--mode=registry` | re-tag + push 到客户 Harbor/Nexus/ACR | 多节点, 已有私有 registry |

**跳过选项**：

| 参数 | 何时使用 |
|------|---------|
| `--skip-longhorn` | 集群已有 Ceph/NFS/CSI-local-path |
| `--skip-juicefs-csi` | 集群已有 JuiceFS CSI 或用其他 RWX 方案 |
| `--skip-image-import` | 镜像已手工导入完成 |

---

## 5. 多环境共存

同一集群可同时跑 dev / test / prod，通过 release 名字 + namespace 天然隔离：

| 资源 | dev (nuwax-rcoder-dev) | prod (nuwax-rcoder-prod) |
|------|----------------------|--------------------------|
| Namespace | `nuwax-rcoder-dev` | `nuwax-rcoder-prod` |
| StorageClass | `juicefs-sc-dev` | `juicefs-sc-prod` |
| ClusterRoleBinding | `rcoder-dev-pods-crb` | `rcoder-prod-pods-crb` |
| ClusterRole | `rcoder-pods-clusterrole` (共享, `helm.sh/resource-policy: keep`) | 同 |
| PostgreSQL | `postgresql` StatefulSet | 同名 (不同 namespace) |
| MinIO | `minio` StatefulSet | 同名 (不同 namespace) |
| rcoder Deployment | `rcoder` Deployment | 同名 (不同 namespace) |
| NodePort | 30080 | 30081 |

---

## 6. 镜像清单

**离线包共 27 个镜像，分 5 类**：

### 6.1 RCoder 自有镜像

| 镜像 | 用途 | 架构 |
|------|------|------|
| `rcoder` | rcoder 主服务 (docker-compose 用, 不进 K8s) | amd64 + arm64 |
| `rcoder-k8s` | rcoder K8s 变体 (`CARGO_FEATURES=kubernetes`) | amd64 + arm64 |
| `rcoder-agent-runner` | 动态 agent-runner 基础镜像 | amd64 + arm64 |

### 6.2 存储层

| 镜像 | 用途 | 版本 |
|------|------|------|
| `postgres:16-alpine` | JuiceFS 元数据 | 16-alpine |
| `minio/minio` | S3 对象存储 + JuiceFS bucket | RELEASE.2024-12-18T13-15-44Z |
| `minio/mc` | MinIO Client (init Job 用) | RELEASE.2024-11-21T17-21-54Z |
| `busybox:1.36` | init container / DNS check | 1.36 |

### 6.3 JuiceFS CE

| 镜像 | 用途 |
|------|------|
| `juicedata/mount:ce-v1.3.1` | JuiceFS FUSE Mount Pod 镜像 |
| `juicedata/juicefs-csi-driver:v0.31.3` | CSI Driver 主镜像 |
| `juicedata/csi-dashboard:v0.31.3` | CSI Dashboard |

### 6.4 JuiceFS CSI Sidecars

| 镜像 | 版本 |
|------|------|
| `registry.k8s.io/sig-storage/csi-node-driver-registrar:v2.9.0` |
| `registry.k8s.io/sig-storage/csi-provisioner:v3.6.0` |
| `registry.k8s.io/sig-storage/csi-resizer:v1.9.0` |
| `registry.k8s.io/sig-storage/livenessprobe:v2.11.0` |

### 6.5 Longhorn v1.7.2

| 镜像 | 用途 |
|------|------|
| `longhornio/longhorn-manager:v1.7.2` | Longhorn 控制平面 |
| `longhornio/longhorn-engine:v1.7.2` | 存储引擎 |
| `longhornio/longhorn-ui:v1.7.2` | Web UI |
| `longhornio/longhorn-instance-manager:v1.7.2` | 实例管理 |
| `longhornio/longhorn-share-manager:v1.7.2` | NFS / iSCSI 共享 |
| `longhornio/backing-image-manager:v1.7.2` | 镜像管理 |
| `longhornio/longhorn-cli:v1.7.2` | CLI |
| `longhornio/support-bundle-kit:v0.0.45` | 诊断工具 |
| `longhornio/csi-attacher:v4.7.0` | CSI attacher |
| `longhornio/csi-provisioner:v4.0.1-20241007` | CSI provisioner |
| `longhornio/csi-resizer:v1.12.0` | CSI resizer |
| `longhornio/csi-snapshotter:v7.0.2-20241007` | CSI snapshotter |
| `longhornio/csi-node-driver-registrar:v2.12.0` | CSI node driver |
| `longhornio/livenessprobe:v2.14.0` | Liveness probe |

---

## 7. 文件结构

```
k8s/
├── docs/
│   └── ARCHITECTURE.md          ← 本文档
│
├── README.md                    ← 总览 + 快速开始
├── nuwax-platform-k8s-plan.md   ← 迁移计划 (build-agent-docker 视角)
│
├── deploy-dev.sh                ← Kustomize dev 部署 (一键)
├── deploy-prod.sh               ← Kustomize prod 部署 (含密码守卫)
├── undeploy.sh                  ← 清理脚本
│
├── helm/                        ← Helm chart (对外交付 + 离线源)
│   └── rcoder/
│       ├── Chart.yaml
│       ├── .helmignore
│       ├── values.yaml           ← 默认值 (dev + prod 共享)
│       ├── values-dev.yaml       ← dev 覆盖 (NodePort 30080, local-path)
│       ├── values-prod.yaml      ← prod 覆盖 (密码 CHANGE-ME)
│       ├── values-offline.yaml   ← 离线 registry 覆盖
│       └── templates/
│           ├── _helpers.tpl      ← 镜像拼装/SC名/ClusterRole名 helpers
│           ├── NOTES.txt
│           ├── storage/
│           │   ├── credentials-secret.yaml
│           │   ├── juicefs-secret.yaml
│           │   ├── juicefs-storageclass.yaml
│           │   ├── juicefs-pvc.yaml          ← RWX PVC (JuiceFS SC)
│           │   ├── postgresql-statefulset.yaml
│           │   ├── postgresql-service.yaml
│           │   ├── minio-statefulset.yaml
│           │   ├── minio-service.yaml
│           │   └── minio-init-job.yaml       ← 创建 juicefs bucket
│           └── rcoder/
│               ├── clusterrole.yaml          ← 集群级, helm.sh/resource-policy: keep
│               ├── clusterrolebinding.yaml
│               ├── serviceaccount.yaml
│               ├── deployment.yaml          ← CONTAINER_RUNTIME=kubernetes
│               ├── service.yaml
│               ├── configmap.yaml           ← config.yml
│               ├── networkpolicy.yaml
│               └── pdb.yaml
│
├── manifests/                   ← Kustomize (日常开发用)
│   ├── base/
│   │   ├── kustomization.yaml
│   │   ├── namespace.yaml
│   │   ├── storage/
│   │   │   ├── kustomization.yaml
│   │   │   ├── juicefs-pvc.yaml
│   │   │   ├── postgresql-deployment.yaml
│   │   │   ├── minio-deployment.yaml
│   │   │   └── minio-init-job.yaml
│   │   └── rcoder/
│   │       ├── kustomization.yaml
│   │       ├── rcoder-deployment.yaml
│   │       ├── rcoder-service.yaml
│   │       ├── rcoder-configmap.yaml
│   │       ├── rcoder-networkpolicy.yaml
│   │       ├── rcoder-pdb.yaml
│   │       └── serviceaccount.yaml
│   ├── overlays/
│   │   ├── dev/
│   │   │   ├── kustomization.yaml
│   │   │   ├── clusterrolebinding.yaml
│   │   │   ├── credentials.yaml
│   │   │   ├── juicefs-secret.yaml
│   │   │   ├── juicefs-storageclass.yaml
│   │   │   └── rcoder-configmap.yaml
│   │   └── prod/
│   │       ├── kustomization.yaml
│   │       ├── clusterrolebinding.yaml
│   │       ├── credentials.yaml
│   │       ├── juicefs-secret.yaml
│   │       ├── juicefs-storageclass.yaml
│   │       └── rcoder-configmap.yaml
│   ├── _deprecated/
│   │   └── nfs/
│   │       ├── nfs-server.yaml
│   │       └── nfs-subdir-provisioner.yaml
│   └── JUICEFS_DEPLOYMENT.md    ← JuiceFS CSI 手动部署指南
│
├── offline/                     ← 离线部署工具
│   ├── images.txt               ← 27 个镜像清单 (版本 pin)
│   ├── install.sh                ← 主安装脚本 (direct / registry 双模式)
│   ├── rewrite-registry.sh      ← registry 模式 re-tag + push
│   └── README.md                ← 客户交付手册
│
├── register2/                   ← 本地私有镜像仓库 (可选)
│   ├── docker-compose.yml        ← registry:2 + 数据卷
│   ├── registry-config.yml
│   └── README.md
│
└── scripts/
    ├── deploy-juicefs.sh         ← 仅部署 JuiceFS CSI (不装 Longhorn)
    ├── test-chat.sh              ← 冒烟测试脚本
    ├── install-k3s-registry-mirrors-cn.sh  ← K3s 镜像加速配置
    └── k3s-registries-cn.yaml    ← 镜像加速配置模板
```

---

## 8. 关键设计决策

### 8.1 rcoder-k8s 独立镜像

**方式 A（采用）**：新增 `rcoder-k8s` 独立镜像 tag
- `Dockerfile` 加 `ARG CARGO_FEATURES`
- `make build-rcoder-k8s` 传 `CARGO_FEATURES=kubernetes`
- 不挂 docker.sock，通过 K8s API 创建动态 agent-runner Pod
- docker-compose 用的 `rcoder` 镜像不受影响

**未采用方式 B**：让 `rcoder` 镜像始终含 kubernetes feature
- docker-compose 老用户也会带上 K8s 代码（轻微浪费）

### 8.2 集群级资源命名策略

| 资源类型 | 命名策略 | 理由 |
|---------|---------|------|
| StorageClass | `juicefs-sc-{release}` | 每个 release 独立，避免多环境冲突 |
| ClusterRole | `rcoder-pods-clusterrole` (固定) | 规则相同，共享最简；`helm.sh/resource-policy: keep` 防误删 |
| ClusterRoleBinding | `{release}-pods-crb` | release 独立 |

### 8.3 密码管理

| 环境 | 策略 |
|------|------|
| dev | 明文默认值（`CHANGE-ME`），开箱即用 |
| prod | `CHANGE-ME-BEFORE-DEPLOY` 占位符，`deploy-prod.sh` 守卫拒绝部署 |
| 进阶 | 推荐 `--set` / SealedSecret / ExternalSecret 注入 |

### 8.4 JuiceFS vs 其他 RWX 方案

| 方案 | 优势 | 劣势 |
|------|------|------|
| JuiceFS + MinIO + PG | K8s 原生，跨节点 POSIX，CSI 集成 | 需要 MinIO + PG 两个有状态组件 |
| NFS Server | 简单 | 单点，无副本，无 CSI 原生集成 |
| CephFS | 成熟，副本分布 | 需要 Ceph 集群，运维复杂 |
| Longhorn NFS | Longhorn 一个组件搞定 | Longhorn NFS 共享盘性能一般 |
|阿里云 NAS/EFS | 云上托管 | 绑定云厂商 |

选择 JuiceFS 的核心原因：RCoder 的核心价值是跨 agent-runner Pod 共享项目文件，JuiceFS 提供 POSIX RWX + K8s CSI 原生支持，是最轻量的自建方案。

---

## 9. 环境要求

| 组件 | 版本要求 | 说明 |
|------|---------|------|
| Kubernetes | 1.19+ | 已在 K3s 1.34.x 测试通过 |
| kubectl | 与集群匹配 | `kubectl cluster-info` 能通 |
| helm | 3.x | 用于 Helm 部署路径 |
| docker | 最新 | 仅 `make k8s-offline-bundle` 或 push 到 register2 时需要 |
| k3s / nerdctl / ctr | 最新 | 仅 direct 模式离线部署需要 |
| open-iscsi | 任意 | Longhorn 依赖 (`apt install open-iscsi`) |

---

## 10. 运维常用命令

```bash
# 查看所有资源
kubectl get all -n nuwax-rcoder-dev

# 查看 Pod 日志
kubectl logs -n nuwax-rcoder-dev -l app=rcoder --tail=200 -f

# 查看 JuiceFS 挂载
kubectl exec -n nuwax-rcoder-dev deploy/rcoder -- df -h | grep juicefs

# 进入 Pod 调试
kubectl exec -it -n nuwax-rcoder-dev deploy/rcoder -- sh

# 查看 PVC 状态
kubectl get pvc -n nuwax-rcoder-dev

# 查看 StorageClass
kubectl get sc | grep -E "longhorn|juicefs"

# 重启 rcoder deployment
kubectl rollout restart deploy/rcoder -n nuwax-rcoder-dev
kubectl rollout status deploy/rcoder -n nuwax-rcoder-dev

# Longhorn UI
kubectl port-forward svc/longhorn-frontend 8080:80 -n longhorn-system

# 清理 dev 环境
ENV=dev ./undeploy.sh
```

---

## 11. 故障排查

### Pod 一直 Pending

```bash
kubectl describe pod -n nuwax-rcoder-dev <pod-name>

# 常见原因:
# - ImagePullBackOff: 节点 containerd 没有镜像 (direct 模式漏了某节点)
# - ContainerCreating: Longhorn / JuiceFS CSI 未就绪
```

### JuiceFS 挂载失败

```bash
# CSI Driver 日志
kubectl logs -n kube-system -l app=juicefs-csi-driver-node --tail=50

# 检查 PostgreSQL / MinIO 是否就绪
kubectl get pods -n nuwax-rcoder-dev -l app=postgresql
kubectl get pods -n nuwax-rcoder-dev -l app=minio

# JuiceFS mount pod 是否 Running
kubectl get pods -n nuwax-rcoder-dev | grep juicefs
```

### Longhorn 无法启动

```bash
# 检查 open-iscsi
ssh <node> "sudo apt install -y open-iscsi && sudo systemctl enable --now iscsid"
kubectl delete pod -n longhorn-system -l app=longhorn-manager --force
```

### rcoder 健康检查失败

```bash
# 查看 rcoder 日志 (RUST_LOG=info)
kubectl logs -n nuwax-rcoder-dev -l app=rcoder --tail=100

# 验证 K8s API 连通性 (rcoder pod 内)
kubectl exec -it -n nuwax-rcoder-dev deploy/rcoder -- sh
kubectl auth can-i create pods -n nuwax-rcoder-dev --as=system:serviceaccount:nuwax-rcoder-dev:rcoder-pods-sa
```

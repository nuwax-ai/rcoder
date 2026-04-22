# RCoder K8s 部署配置

本目录包含在 Kubernetes 环境中部署 RCoder 所需的完整配置，支持 **Kustomize** 和 **Helm** 两种部署方式。

## 目录结构

```
k8s/
├── manifests/                    # Kustomize 部署方式 (开发环境)
│   ├── base/                    # 基础配置
│   │   ├── storage/
│   │   └── rcoder/
│   └── overlays/               # 环境覆盖
│       ├── dev/                # 开发环境 (nuwax-rcoder-dev)
│       └── prod/               # 生产环境 (nuwax-rcoder-prod)
│
├── helm/                        # Helm 部署方式 (生产环境)
│   └── rcoder/
│       ├── Chart.yaml
│       ├── values.yaml         # 默认配置
│       ├── values-dev.yaml     # 开发环境配置
│       ├── values-prod.yaml    # 生产环境配置
│       ├── templates/          # 资源模板
│       ├── Makefile            # Helm Makefile
│       └── scripts/            # Helm 部署脚本
│           └── helm-deploy.sh  # Helm 部署脚本
│
├── _deprecated/nfs/             # 废弃的 NFS 配置
│
├── deploy.sh                    # Helm 生产环境一键部署
├── deploy-dev.sh               # Kustomize 开发环境一键部署
├── deploy-juicefs.sh           # (已废弃) 存储层部署脚本
├── test-chat.sh               # 测试脚本
└── README.md                  # 本文档
```

---

## 部署方式

### 开发环境 (Kustomize) - 快速迭代

```bash
# 一键部署 (包含 Longhorn + JuiceFS CSI + RCoder)
./deploy-dev.sh

# 常用命令
kubectl apply -k manifests/overlays/dev    # 部署/更新
kubectl delete -k manifests/overlays/dev  # 删除
kubectl get all -n nuwax-rcoder-dev        # 查看状态
```

---

### 生产环境 (Helm) - 版本管理

```bash
# 一键部署 (包含 Longhorn + JuiceFS CSI + RCoder)
./deploy.sh

# 或直接使用 Helm 脚本
cd helm/rcoder && ./scripts/helm-deploy.sh install

# 常用命令
helm list -n nuwax-rcoder                  # 查看 release
helm/rcoder/scripts/helm-deploy.sh upgrade  # 升级
helm rollback rcoder -n nuwax-rcoder       # 回滚
helm/rcoder/scripts/helm-deploy.sh uninstall  # 卸载
```

#### Helm Makefile (在 helm/rcoder 目录下执行)

```bash
cd helm/rcoder

make install          # 安装 (默认 values.yaml)
make install-dev      # 安装 (开发环境)
make install-prod     # 安装 (生产环境)
make upgrade          # 升级
make uninstall        # 卸载
make status           # 查看 release 状态
make template         # 渲染模板 (不部署)
make package          # 打包 Chart
```

---

### Makefile 方式

```bash
# 开发环境
make dev-build-k8s     # 构建镜像
make dev-up-k8s        # 部署
make dev-down-k8s      # 清理

# 生产环境
make helm-up-prod       # Helm 生产部署
make helm-upgrade       # 升级
make helm-down          # 卸载
```

```bash
# 构建镜像
make dev-build-k8s

# 部署
make dev-up-k8s

# 重启
make dev-restart-k8s

# 清理
make dev-down-k8s

# 查看日志
make dev-logs-k8s
```

---

## 环境配置

### Helm values 文件

| 文件 | 用途 |
|------|------|
| `values.yaml` | 默认配置 |
| `values-dev.yaml` | 开发环境 (小资源) |
| `values-prod.yaml` | 生产环境 (高可用) |

### 自定义配置

创建 `values-custom.yaml`：

```yaml
# 镜像配置
image:
  registry: your-registry.com
  repository: rcoder
  tag: v1.0.0

# 命名空间
namespace: my-rcoder

# 副本数
replicaCount: 3

# 资源限制
resources:
  requests:
    cpu: 1000m
    memory: 2Gi
  limits:
    cpu: 4000m
    memory: 8Gi

# 使用外部 PostgreSQL
postgresql:
  enabled: false
  external: true
  host: "postgres.example.com"
  port: 5432
  auth:
    database: juicefs
    username: juicefs
    password: "your-password"

# 使用外部 S3
minio:
  enabled: false
  external: true
  endpoint: "https://s3.example.com"
  accessKey: "your-access-key"
  secretKey: "your-secret-key"
```

部署：

```bash
helm upgrade --install rcoder ./helm/rcoder \
  --values ./helm/rcoder/values.yaml \
  --values values-custom.yaml \
  --namespace my-rcoder \
  --create-namespace \
  --wait
```

---

## 存储架构

```
┌─────────────────────────────────────────────────────────────┐
│                     Kubernetes 集群                          │
│  ┌──────────────────────────────────────────────────────┐  │
│  │            JuiceFS CSI Driver                         │  │
│  │         (StorageClass: juicefs-sc)                    │  │
│  └──────────────────────────────────────────────────────┘  │
│                            ↓                                │
│  ┌────────────────┐    ┌────────────────┐                │
│  │ rcoder 主服务   │    │ Agent Runner   │                │
│  │ Pod (Deployment)│    │ Pod (动态创建)  │                │
│  └────────┬───────┘    └────────┬───────┘                │
│           │                      │                         │
│           └──────────┬───────────┘                         │
│                      ↓                                     │
│              ┌──────────────┐                              │
│              │ JuiceFS 卷   │  ← 共享文件系统 (RWX)        │
│              └───────┬──────┘                              │
└──────────────────────┼────────────────────────────────────┘
                       ↓
┌───────────────────────────────────────────────────────────────┐
│  MinIO (S3) ←──── 数据存储                                  │
│  PostgreSQL ← 元数据存储                                     │
└───────────────────────────────────────────────────────────────┘
```

---

## 环境要求

- Kubernetes 1.14+
- kubectl 已配置
- Helm 3.x (使用 Helm 时)
- Longhorn 存储 (提供持久化存储，支持快照和备份)
- JuiceFS CSI Driver (使用 JuiceFS 时)

### 部署 Longhorn 存储

```bash
# 部署 Longhorn (单节点或多节点集群)
kubectl apply -f https://raw.githubusercontent.com/longhorn/longhorn/master/deploy/longhorn.yaml

# 或使用 Helm
helm repo add longhorn https://charts.longhorn.io
helm install longhorn longhorn/longhorn -n longhorn-system --create-namespace
```

### 部署 JuiceFS CSI Driver

```bash
helm repo add juicefs https://juicefs.github.io/charts
helm install juicefs-csi-driver juicefs/juicefs-csi-driver \
  --namespace kube-system \
  --set webhook.enabled=false
```

---

## 故障排查

```bash
# 查看 Helm release 状态
helm status rcoder -n nuwax-rcoder

# 查看 Pod 状态
kubectl get pods -n nuwax-rcoder

# 查看日志
kubectl logs -n nuwax-rcoder -l app=rcoder --tail=100

# 查看 PVC
kubectl get pvc -n nuwax-rcoder

# 查看 Events
kubectl get events -n nuwax-rcoder --sort-by='.lastTimestamp'
```

---

## 生产环境部署 (K3s)

```bash
# 1. 安装 K3s (中国镜像)
curl -sfL https://rancher-mirror.rancher.cn/k3s/k3s-install.sh | INSTALL_K3S_MIRROR=cn sh -

# 2. 确保 K3s 集群已配置 kubectl
kubectl get nodes

# 2. 一键部署 (自动安装 Longhorn + JuiceFS CSI + RCoder)
./deploy.sh

# 3. 查看状态
kubectl get pods -n nuwax-rcoder

# 4. 测试
./test-chat.sh
```

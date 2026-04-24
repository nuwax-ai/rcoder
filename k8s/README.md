# RCoder K8s 部署配置

本目录包含在 Kubernetes 环境中部署 RCoder 所需的完整配置，使用 **Kustomize** 做 dev/prod 环境隔离。

## 目录结构

```
k8s/
├── manifests/                    # Kustomize manifests
│   ├── base/                    # 基础配置（不可直接部署，需走 overlay）
│   │   ├── storage/             # postgresql / minio / juicefs-init / juicefs-pvc
│   │   └── rcoder/              # rcoder Deployment/Service/NetworkPolicy/PDB/SA+ClusterRole
│   └── overlays/
│       ├── dev/                 # 开发环境 (nuwax-rcoder-dev, NodePort 30080)
│       └── prod/                # 生产环境 (nuwax-rcoder-prod, NodePort 30081)
│
├── deploy-dev.sh                 # [核心] Kustomize 开发环境一键部署
├── deploy-prod.sh                # [核心] Kustomize 生产环境一键部署（含占位符密码守卫）
├── undeploy.sh                   # [核心] 清理部署（ENV=dev|prod）
│
├── scripts/                      # 辅助脚本
│   ├── deploy-juicefs.sh         # 仅部署存储层 (ENV=dev|prod)
│   ├── test-chat.sh              # 功能冒烟测试 (NAMESPACE 可覆盖)
│   ├── install-k3s-registry-mirrors-cn.sh  # K3s 节点镜像加速配置
│   └── k3s-registries-cn.yaml    # 镜像加速配置模板
│
└── README.md
```

dev 和 prod 可以并存于同一个集群：
- 集群级资源（StorageClass / ClusterRoleBinding）命名独立：`juicefs-sc-dev` / `juicefs-sc-prod`、`rcoder-pods-crb-dev` / `rcoder-pods-crb-prod`
- NodePort 错开：dev=30080 / prod=30081
- ClusterRole `rcoder-pods-clusterrole` 是唯一共享的集群级资源（两个 overlay 写入完全一致，幂等）

---

## 快速开始

### 开发环境

```bash
# 首次完整部署（含 Longhorn、JuiceFS CSI、open-iscsi 检查）
./k8s/deploy-dev.sh

# 常用
kubectl apply -k k8s/manifests/overlays/dev     # 部署/更新
kubectl delete -k k8s/manifests/overlays/dev    # 删除
kubectl get all -n nuwax-rcoder-dev             # 查看状态
k8s/scripts/test-chat.sh                        # 冒烟测试
```

### 生产环境

> **重要**：首次部署前必须替换 `k8s/manifests/overlays/prod/credentials.yaml` 和 `juicefs-secret.yaml` 里的 `CHANGE-ME-BEFORE-DEPLOY` 占位符；`deploy-prod.sh` 有守卫检查，发现占位符会拒绝部署。若使用 SealedSecret / ExternalSecret 在集群外注入，设置 `FORCE_PROD_DEPLOY=1` 跳过。

```bash
# 首次完整部署
./k8s/deploy-prod.sh

# 跳过占位符检查（仅当通过外部密钥注入方案时）
FORCE_PROD_DEPLOY=1 ./k8s/deploy-prod.sh

# 常用
kubectl apply -k k8s/manifests/overlays/prod
kubectl get all -n nuwax-rcoder-prod
NAMESPACE=nuwax-rcoder-prod k8s/scripts/test-chat.sh
```

### 清理

```bash
ENV=dev  ./k8s/undeploy.sh      # 清理 dev
ENV=prod ./k8s/undeploy.sh      # 清理 prod
```

---

## Makefile 快捷命令

在项目根目录执行：

```bash
# 镜像
make dev-build-k8s             # 构建并推送 K8s 镜像

# 部署/清理（默认走 overlay）
make deploy-dev                # kubectl apply -k overlays/dev
make deploy-prod               # kubectl apply -k overlays/prod
make undeploy-dev              # 清理 dev
make undeploy-prod             # 清理 prod

# 开发闭环
make dev-up-k8s                # 部署 dev
make dev-restart-k8s           # 重新构建镜像 + 重部署 dev
make dev-down-k8s              # 清理 dev
make dev-logs-k8s              # 跟随 rcoder 日志

# 本地验证（不部署）
make kustomize-build           # 分别 build base / dev / prod
```

---

## 存储架构

```
┌─────────────────────────────────────────────────────────────┐
│                     Kubernetes 集群                          │
│  ┌──────────────────────────────────────────────────────┐  │
│  │            JuiceFS CSI Driver (kube-system)           │  │
│  └──────────────────────────────────────────────────────┘  │
│                            ↓                                │
│  ┌────────────────┐    ┌────────────────┐                   │
│  │ rcoder 主服务   │    │ Agent Runner   │                   │
│  │ (Deployment)   │    │ (动态创建 Pod) │                    │
│  └────────┬───────┘    └────────┬───────┘                   │
│           └──────────┬───────────┘                          │
│                      ↓                                      │
│              ┌──────────────┐                               │
│              │ JuiceFS 卷   │  ← ReadWriteMany 共享         │
│              └───────┬──────┘                               │
└──────────────────────┼──────────────────────────────────────┘
                       ↓
┌───────────────────────────────────────────────────────────────┐
│  MinIO (S3)    ← JuiceFS 数据存储（per-namespace）             │
│  PostgreSQL    ← JuiceFS 元数据存储（per-namespace）           │
└───────────────────────────────────────────────────────────────┘
```

每个 namespace 独立拥有 PostgreSQL 和 MinIO 实例，metadata 和 object 存储互不干扰。

---

## 环境要求

- Kubernetes 1.14+（本仓库在 K3s 1.34.x 上测试）
- kubectl 已配置 kubeconfig
- Helm（仅用于集群首次安装 JuiceFS CSI Driver）
- Longhorn 存储（为 PostgreSQL/MinIO 提供 PV；`deploy-*.sh` 会自动安装）
- `open-iscsi`（Longhorn 依赖；`deploy-*.sh` 会自动安装）

国内节点建议先配置 k3s 镜像加速：
```bash
sudo k8s/scripts/install-k3s-registry-mirrors-cn.sh
```

---

## 故障排查

```bash
# Pod 状态
kubectl get pods -n nuwax-rcoder-dev   # 或 -prod

# 事件
kubectl get events -n nuwax-rcoder-dev --sort-by='.lastTimestamp'

# RCoder 日志
kubectl logs -n nuwax-rcoder-dev -l app=rcoder --tail=200 -f

# PVC 绑定情况
kubectl get pvc -n nuwax-rcoder-dev
kubectl describe pvc rcoder-workspace -n nuwax-rcoder-dev

# StorageClass
kubectl get sc
```

---

## 从 Helm 迁移（历史说明）

此前仓库曾提供 `k8s/helm/rcoder/` Helm chart 作为另一条部署路径，现已删除。所有 K8s 部署统一走 Kustomize：
- 原 `deploy.sh`（Helm-based）→ 重写为 `deploy-prod.sh`（Kustomize-based）
- `make helm-*` 目标已移除；改用 `make deploy-dev` / `make deploy-prod`

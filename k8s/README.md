# Kubernetes 部署配置

本目录包含在 Kubernetes 环境中部署 RCoder 所需的完整配置。

## 环境要求

- K8s 集群（本项目基于 **K3s** 开发测试，验证 StorageClass 为 `local-path`）
- kubectl 已配置并能访问集群
- `local-path` StorageClass 可用（K3s 默认存在）
- Docker 镜像已推送到可访问的镜像仓库

### 确认 local-path StorageClass 可用

```bash
kubectl get storageclass

# 预期输出（K3s 默认）:
# NAME                   PROVISIONER
# local-path (default)   rancher.io/local-path
```

> 如果集群没有 `local-path`，需要先安装：
> ```bash
> kubectl apply -f https://raw.githubusercontent.com/rancher/local-path-provisioner/master/deploy/local-path-storage.yaml
> ```

---

## 存储架构

RCoder K8s 部署使用 **NFS 共享存储**，支持两种模式：

### 模式一：自建 NFS（默认）

使用 K8s 集群内自建的 NFS Server，数据持久化到 `local-path` PV：

```
nfs-server StatefulSet
    └── nfs-server-pvc (100Gi, local-path) ← 持久化存储
            └── /exports
                    ├── /rcoder/rcoder-workspace/          ← RCoder Deployment
                    ├── /rcoder/rcoder-agent-{pid}/       ← 动态 Agent Pod
                    └── /rcoder/computer-agent-runner-{uid}/
```

### 模式二：外部 NFS（多租户）

客户已有 NFS Server 时，只需要在部署时修改 NFS 相关环境变量即可，无需部署 `nfs-server.yaml`。

---

## 快速开始

### 1. 构建镜像并部署

```bash
# 构建镜像（启用 kubernetes feature）
make dev-build-k8s

# 部署（自动部署 NFS 存储层 + RCoder 应用层）
make dev-up-k8s
```

> 如果镜像已存在且不需要重新构建，可直接：
> ```bash
> make dev-up-k8s
> ```

### 2. 检查部署状态

```bash
# 查看所有 Pod（包含 NFS 存储层）
kubectl get pods -n nfs-storage
kubectl get pods -n rcoder

# 查看 NFS StorageClass
kubectl get storageclass | grep rcoder-nfs

# 查看 PVC
kubectl get pvc -n nfs-storage
kubectl get pvc -n rcoder

# 查看 RCoder 日志
kubectl logs -n rcoder -l app=rcoder -f

# 查看 NFS Server 日志
kubectl logs -n nfs-storage -l app=nfs-server -f
```

### 3. 测试

```bash
./test-chat.sh
```

### 4. 清理

```bash
# 与 make dev-up-k8s 对称的完整卸载
make dev-down-k8s

# 或使用脚本
./undeploy.sh
```

---

## 手动部署步骤

当 `make dev-up-k8s` 不适用时（如客户环境），按以下顺序手动部署。

### 第一步：确认 local-path StorageClass

```bash
kubectl get storageclass | grep local-path
```

### 第二步：部署 NFS 存储层（模式一自建 NFS）

```bash
kubectl apply -f manifests/nfs-server.yaml

# 等待 NFS Server 就绪（关键！）
kubectl get pods -n nfs-storage -w
# 等待 nfs-server-0 显示 1/1 Running
# 等待 nfs-client-provisioner-xxxxx 显示 1/1 Running
```

验证 NFS 存储层：

```bash
# 确认 StorageClass 已创建
kubectl get storageclass | grep rcoder-nfs

# 确认 PVC 已绑定
kubectl get pvc -n nfs-storage
# 预期: nfs-server-pvc 状态为 Bound

# 测试 NFS 挂载（可选）
kubectl apply -f - <<'EOF'
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: nfs-test
  namespace: nfs-storage
spec:
  accessModes: ["ReadWriteMany"]
  storageClassName: rcoder-nfs
  resources:
    requests:
      storage: 1Gi
EOF
kubectl get pvc nfs-test -n nfs-storage
```

### 第三步：部署 RCoder 应用层

```bash
# 按顺序部署
kubectl apply -f manifests/namespace.yaml
kubectl apply -f manifests/serviceaccount.yaml
kubectl apply -f manifests/rcoder-configmap.yaml
kubectl apply -f manifests/rcoder-pvc.yaml           # RCoder Deployment 的工作空间 PVC
kubectl apply -f manifests/rcoder-deployment.yaml    # 替换镜像标签: sed -i 's|image: rcoder:test|image: <your-image>|' manifests/rcoder-deployment.yaml
kubectl apply -f manifests/rcoder-service.yaml
kubectl apply -f manifests/rcoder-networkpolicy.yaml
kubectl apply -f manifests/rcoder-pdb.yaml

# 等待就绪
kubectl rollout status deploy/rcoder -n rcoder --timeout=180s
```

### 第四步：验证部署

```bash
# 检查所有 Pod
kubectl get pods -n nfs-storage
kubectl get pods -n rcoder

# 测试健康检查
kubectl exec -it deploy/rcoder -n rcoder -- wget -qO- http://localhost:8087/health

# 测试 API
curl http://<node-ip>:30080/health
```

---

## 外部 NFS 部署（模式二：多租户）

客户已有 NFS Server 时，跳过 `nfs-server.yaml`，只部署应用层并修改 NFS 配置。

### 1. 客户提供 NFS 信息

| 配置项 | 示例值 |
|--------|--------|
| NFS Server 地址 | `nfs.customer.com` 或 `192.168.1.100` |
| NFS 共享路径 | `/shared/rcoder` |
| StorageClass（可选） | 客户已有 NFS Subdir Provisioner 的 SC |

### 2. 修改配置

修改 `manifests/rcoder-deployment.yaml` 中的环境变量：

```yaml
env:
# NFS 存储配置（替换为客户的 NFS Server）
- name: RCODER_K8S_NFS_SERVER
  value: "nfs.customer.com"       # ← 客户 NFS 地址
- name: RCODER_K8S_NFS_PATH
  value: "/shared/rcoder"         # ← 客户 NFS 路径
- name: RCODER_K8S_STORAGE_CLASS
  value: "customer-nfs"          # ← 客户 StorageClass（可选）
```

修改 `manifests/rcoder-pvc.yaml` 中的 StorageClass：

```yaml
spec:
  storageClassName: customer-nfs  # ← 客户的 StorageClass
```

修改 `manifests/rcoder-configmap.yaml` 中的 NFS 配置：

```yaml
kubernetes_config:
  nfs_server: "nfs.customer.com"
  nfs_path: "/shared/rcoder"
  storage_class: "customer-nfs"
```

### 3. 部署（跳过 NFS 存储层）

```bash
# 不执行 kubectl apply -f manifests/nfs-server.yaml
kubectl apply -f manifests/namespace.yaml
kubectl apply -f manifests/serviceaccount.yaml
kubectl apply -f manifests/rcoder-configmap.yaml
kubectl apply -f manifests/rcoder-pvc.yaml
kubectl apply -f manifests/rcoder-deployment.yaml
kubectl apply -f manifests/rcoder-service.yaml
```

---

## 环境变量

### 运行时配置

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `CONTAINER_RUNTIME` | `kubernetes` | 启用 K8s 运行时模式 |
| `RCODER_K8S_NAMESPACE` | `rcoder` | 动态 Pod 创建的 namespace |
| `IMAGE` | `rcoder:test-k8s` | 部署镜像标签 |
| `ROLLOUT_TIMEOUT` | `180s` | rollout 等待超时 |

### NFS 存储配置

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `RCODER_K8S_NFS_SERVER` | `nfs-server.nfs-storage.svc.cluster.local` | NFS Server 地址 |
| `RCODER_K8S_NFS_PATH` | `/exports` | NFS 共享路径 |
| `RCODER_K8S_STORAGE_CLASS` | `rcoder-nfs` | StorageClass 名称 |

---

## 目录结构

```
k8s/
├── README.md                      # 本文档
├── undeploy.sh                    # 卸载脚本
├── test-chat.sh                   # 测试脚本
├── kind-config.yaml               # Kind 集群配置
├── start-kind.sh                  # 启动 Kind 集群脚本
└── manifests/
    ├── namespace.yaml            # Namespace 定义 (rcoder)
    ├── serviceaccount.yaml       # ServiceAccount + RBAC + ClusterRole
    ├── rcoder-configmap.yaml     # RCoder 配置文件 (ConfigMap)
    ├── rcoder-deployment.yaml    # RCoder Deployment
    ├── rcoder-service.yaml       # RCoder Service (NodePort 30080)
    ├── rcoder-pvc.yaml           # RCoder 工作空间 PVC (50Gi)
    ├── rcoder-networkpolicy.yaml  # 网络策略
    ├── rcoder-pdb.yaml          # PodDisruptionBudget
    └── nfs-server.yaml           # NFS 存储层（自建 NFS 模式）
        ├── nfs-storage Namespace
        ├── nfs-server StatefulSet (NFS Server)
        ├── nfs-server-pvc (100Gi, local-path) ← NFS 数据持久化
        ├── nfs-client-provisioner (NFS Subdir External Provisioner)
        ├── StorageClass (rcoder-nfs)
        └── rcoder-workspace PVC (共享工作空间)
```

---

## 故障排查

### NFS 相关

#### NFS Server 一直 Pending

```bash
# 检查 PVC 状态
kubectl get pvc -n nfs-storage -o wide

# 检查 StorageClass
kubectl get storageclass | grep rcoder-nfs

# 检查 local-path PV 是否正常
kubectl get pv | grep nfs-server
```

#### NFS Provisioner 报错

```bash
# 查看 provisioner 日志
kubectl logs -n nfs-storage -l app=nfs-client-provisioner

# 检查 NFS Server 是否可达（从 provisioner pod 内）
kubectl exec -it -n nfs-storage deploy/nfs-client-provisioner -- sh
telnet nfs-server.nfs-storage.svc.cluster.local 2049
```

#### Pod 挂载 NFS 失败

```bash
# 从 Pod 内测试 NFS 挂载
kubectl exec -it -n rcoder deploy/rcoder -- sh
mount -t nfs4 nfs-server.nfs-storage.svc.cluster.local:/exports /tmp/test
ls /tmp/test
umount /tmp/test
```

### RCoder 相关

#### Pod 无法创建

```bash
# 检查 RBAC 权限
kubectl auth can-i create pods --as=system:serviceaccount:rcoder:rcoder-pods-sa
kubectl auth can-i delete pods --as=system:serviceaccount:rcoder:rcoder-pods-sa
kubectl auth can-i get pods --as=system:serviceaccount:rcoder:rcoder-pods-sa

# 查看 events
kubectl get events -n rcoder --sort-by='.lastTimestamp'
```

#### 镜像拉取失败

```bash
# 检查镜像是否存在
kubectl describe pod -n rcoder -l app=rcoder | grep -A 5 "Failed to pull image"

# 拉取到节点（针对私有仓库）
docker pull <registry>/rcoder:latest
kind load docker-image <registry>/rcoder:latest --name rcoder
```

#### 无法连接到 K8s API

```bash
# 检查 in-cluster 配置
kubectl exec -it -n rcoder deploy/rcoder -- cat /var/run/secrets/kubernetes.io/serviceaccount/token

# 检查 ServiceAccount 是否绑定正确
kubectl get sa rcoder-pods-sa -n rcoder -o yaml
```

### 查看完整日志

```bash
# RCoder 主服务
kubectl logs -n rcoder -l app=rcoder --tail=100 -f

# NFS Server
kubectl logs -n nfs-storage -l app=nfs-server --tail=50 -f

# NFS Provisioner
kubectl logs -n nfs-storage -l app=nfs-client-provisioner --tail=50 -f
```

---

## Kind 集群本地测试

如需在本地 Kind 集群测试：

```bash
# 1. 创建 Kind 集群（已配置端口映射）
kind create cluster --config kind-config.yaml

# 2. 构建镜像并部署
make dev-build-k8s
make dev-up-k8s

# 3. 测试
./test-chat.sh

# 4. 清理
make dev-down-k8s
kind delete cluster --name rcoder-dev
```

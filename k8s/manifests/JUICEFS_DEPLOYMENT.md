# JuiceFS + MinIO + PostgreSQL 部署指南

## 架构

```
┌─────────────────────────────────────────────────────────────┐
│                     Kubernetes 集群                          │
│                                                              │
│  ┌──────────────────────────────────────────────────────┐  │
│  │            JuiceFS CSI Driver                         │  │
│  │         (StorageClass: juicefs-sc)                  │  │
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

## 文件结构

```
manifests/
├── 00-namespace.yaml              # Namespace
├── 01-storage/                    # 存储层
│   ├── postgresql-deployment.yaml
│   ├── minio-deployment.yaml
│   ├── minio-init-job.yaml
│   ├── juicefs-secret.yaml
│   ├── juicefs-storageclass.yaml
│   └── juicefs-pvc.yaml
├── 02-rcoder/                    # 应用层
│   ├── serviceaccount.yaml
│   ├── rcoder-configmap.yaml
│   ├── rcoder-deployment.yaml
│   ├── rcoder-service.yaml
│   ├── rcoder-networkpolicy.yaml
│   └── rcoder-pdb.yaml
└── _deprecated/nfs/               # 废弃的 NFS 配置
```

## 部署顺序

### 第一步：部署 JuiceFS CSI Driver

```bash
# 使用 Helm 部署
helm repo add juicefs https://juicefs.github.io/charts
helm repo update

helm install juicefs-csi-driver juicefs/juicefs-csi-driver \
  --namespace kube-system \
  --set webhook.enabled=false \
  --set csiSidecarImage.registry=docker.io \
  --set csiSidecarImage.repository=juicedata/csi-sidecar \
  --set csiSidecarImage.tag=v1.3.8 \
  --set image.registry=docker.io \
  --set image.repository=juicedata/juicefs-csi-driver \
  --set image.tag=v1.1.0 \
  --set nodeDaemonImage.registry=docker.io \
  --set nodeDaemonImage.repository=juicedata/juicefs-csi-driver \
  --set nodeDaemonImage.tag=v1.1.0
```

验证：

```bash
kubectl get pods -n kube-system | grep juicefs
# 预期：juicefs-csi-driver-xxx 运行正常
```

---

### 第二步：部署 PostgreSQL

```bash
kubectl apply -f manifests/01-storage/postgresql-deployment.yaml
```

验证：

```bash
kubectl get pods -n nuwax-rcoder | grep postgresql
kubectl logs -n nuwax-rcoder -l app=postgresql --tail=50
```

---

### 第三步：部署 MinIO

```bash
kubectl apply -f manifests/01-storage/minio-deployment.yaml
```

验证：

```bash
kubectl get pods -n nuwax-rcoder | grep minio
# 访问 MinIO Console: http://<minio-ip>:9001
```

---

### 第四步：初始化 MinIO Bucket

```bash
kubectl apply -f manifests/01-storage/minio-init-job.yaml

# 等待完成
kubectl wait --for=condition=complete job/minio-init -n nuwax-rcoder --timeout=120s

# 验证
kubectl exec -it -n nuwax-rcoder deploy/minio -- mc ls myminio/
```

---

### 第五步：创建 JuiceFS Secret

```bash
kubectl apply -f manifests/01-storage/juicefs-secret.yaml
```

---

### 第六步：创建 JuiceFS StorageClass

```bash
kubectl apply -f manifests/01-storage/juicefs-storageclass.yaml
```

验证：

```bash
kubectl get sc | grep juicefs
```

---

### 第七步：部署 RCoder 相关资源

```bash
# 部署 rcoder PVC (使用 JuiceFS)
kubectl apply -f manifests/01-storage/juicefs-pvc.yaml

# 验证 PVC
kubectl get pvc -n nuwax-rcoder
# 状态应为 Bound

# 部署其他 rcoder 资源
kubectl apply -f manifests/00-namespace.yaml
kubectl apply -f manifests/02-rcoder/serviceaccount.yaml
kubectl apply -f manifests/02-rcoder/rcoder-configmap.yaml
kubectl apply -f manifests/02-rcoder/rcoder-deployment.yaml
kubectl apply -f manifests/02-rcoder/rcoder-service.yaml
kubectl apply -f manifests/02-rcoder/rcoder-networkpolicy.yaml
kubectl apply -f manifests/02-rcoder/rcoder-pdb.yaml
```

---

### 第八步：验证整个系统

```bash
# 检查所有 Pod
kubectl get pods -n nuwax-rcoder

# 查看 JuiceFS 挂载情况
kubectl exec -it -n nuwax-rcoder deploy/rcoder -- df -h | grep juicefs

# 测试写入
kubectl exec -it -n nuwax-rcoder deploy/rcoder -- sh -c "echo 'test' > /app/project_workspace/test.txt"

# 查看日志
kubectl logs -n nuwax-rcoder -l app=rcoder --tail=50
```

---

## 存储结构

部署后，数据存储结构如下：

```
PostgreSQL (元数据):
├── juicefs database
│   ├── inodes
│   ├── chunks
│   ├── del-files
│   └── sessions

MinIO (S3 数据):
├── juicefs bucket
│   └── juicefs/
│       ├── juicefs.db        ← JuiceFS 内部数据库
│       └── chunk-xxx         ← 数据块
```

---

## 卸载顺序

```bash
# 1. 删除 rcoder 资源
kubectl delete -f manifests/02-rcoder/rcoder-deployment.yaml
kubectl delete -f manifests/02-rcoder/rcoder-configmap.yaml
kubectl delete -f manifests/01-storage/juicefs-pvc.yaml

# 2. 删除 JuiceFS 相关
kubectl delete -f manifests/01-storage/juicefs-storageclass.yaml
kubectl delete -f manifests/01-storage/juicefs-secret.yaml

# 3. 删除存储层
kubectl delete -f manifests/01-storage/minio-deployment.yaml
kubectl delete -f manifests/01-storage/postgresql-deployment.yaml

# 4. 删除 JuiceFS CSI Driver (可选)
helm uninstall juicefs-csi-driver -n kube-system
```

---

## 常见问题

### 1. PVC 一直 Pending

```bash
# 检查 JuiceFS CSI Driver 状态
kubectl get pods -n kube-system | grep juicefs

# 检查 StorageClass
kubectl get sc juicefs-sc -o yaml

# 检查 Secret
kubectl get secret juicefs-secret -n nuwax-rcoder -o yaml
```

### 2. MinIO Bucket 创建失败

```bash
# 确认 MinIO 运行正常
kubectl logs -n nuwax-rcoder -l app=minio --tail=20

# 手动进入 Pod 测试
kubectl exec -it -n nuwax-rcoder deploy/minio -- sh
mc alias set myminio http://localhost:9000 minioadmin minioadmin
mc mb myminio/juicefs
```

### 3. JuiceFS 挂载失败

```bash
# 查看 Node Pod 日志
kubectl get pods -n kube-system -o wide | grep juicefs

# 查看挂载详情
kubectl describe pod -n kube-system -l app=juicefs-csi-driver-node
```

---

## 资源需求

| 组件 | CPU | Memory | Storage |
|------|-----|--------|---------|
| PostgreSQL | 500m | 1Gi | 20Gi |
| MinIO | 500m | 1Gi | 100Gi+ |
| JuiceFS CSI | 100m | 128Mi | - |

总计：约 2 核 + 2.5Gi + 120Gi 存储

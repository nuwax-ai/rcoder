# Kubernetes 部署配置

本目录包含在 Kubernetes 环境中部署 RCoder 所需的配置。

## 前置条件

- 已有运行的 K8s 集群
- kubectl 已配置并能访问集群
- Docker 镜像已推送到可访问的镜像仓库（可选，如使用 `imagePullPolicy: Always`）

## 快速开始

### 1. 部署 RCoder

```bash
# 标准部署（推荐）
make dev-up-k8s
```

重新构建并重启（代码变更后）：

```bash
make dev-restart-k8s IMAGE=rcoder:test-k8s
```

### 2. 检查部署状态

```bash
# 查看 Pod 状态
kubectl get pods -n rcoder

# 查看日志
kubectl logs -n rcoder -l app=rcoder -f
```

### 3. 测试

```bash
./test-chat.sh
```

### 4. 清理

与 `make dev-up-k8s` 对称的完整卸载（Deployment、Service、运行时创建的 Pod、RBAC、Namespace）：

```bash
make dev-down-k8s
```

或在 `k8s/` 目录下使用脚本（逻辑与上面一致）：

```bash
./undeploy.sh
```

## 目录结构

```
k8s/
├── README.md                    # 本文档
├── undeploy.sh                  # 卸载脚本
├── test-chat.sh                 # 测试脚本
└── manifests/
    ├── namespace.yaml           # Namespace 定义
    ├── serviceaccount.yaml     # ServiceAccount + RBAC
    ├── rcoder-deployment.yaml  # RCoder Deployment
    └── rcoder-service.yaml     # RCoder Service (NodePort)
```

## 手动部署步骤

### 1. 创建 Namespace

```bash
kubectl apply -f manifests/namespace.yaml
```

### 2. 配置 RBAC

```bash
kubectl apply -f manifests/serviceaccount.yaml
```

### 3. 验证 RBAC

```bash
# 检查权限
kubectl auth can-i create pods --as=system:serviceaccount:rcoder:rcoder-pods-sa
kubectl auth can-i delete pods --as=system:serviceaccount:rcoder:rcoder-pods-sa
kubectl auth can-i get pods --as=system:serviceaccount:rcoder:rcoder-pods-sa
```

### 4. 部署 RCoder

```bash
kubectl apply -f manifests/rcoder-deployment.yaml
kubectl apply -f manifests/rcoder-service.yaml
```

## 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `IMAGE` | `rcoder:test-k8s` | 部署镜像标签 |
| `ROLLOUT_TIMEOUT` | `180s` | rollout 等待超时 |
| `CONTAINER_RUNTIME` | `kubernetes` | 启用 K8s 运行时 |
| `RCODER_K8S_NAMESPACE` | `rcoder` | Pod 创建的 namespace（需与 Deployment namespace 一致） |

## 故障排查

### Pod 无法创建

```bash
# 检查 RBAC 权限
kubectl auth can-i create pods --as=system:serviceaccount:rcoder:rcoder-pods-sa

# 查看 events
kubectl get events -n rcoder --sort-by='.lastTimestamp'
```

### 无法连接到 K8s API

```bash
# 检查 in-cluster 配置
kubectl exec -it -n rcoder deploy/rcoder -- sh
# 在 pod 内执行
cat /var/run/secrets/kubernetes.io/serviceaccount/token
```

### 镜像拉取失败

```bash
# 如使用本地镜像，需要先加载镜像到节点
# 或者配置 imagePullSecrets 访问私有仓库
```

## Kind 集群（如需本地测试）

如需使用 Kind 在本地创建测试集群：

```bash
# 创建 Kind 集群
kind create cluster --config kind-config.yaml

# 构建镜像并部署
make dev-restart-k8s IMAGE=rcoder:test-k8s
```

# Kubernetes RBAC Configuration

**Date**: 2026-04-16  
**Feature**: K8s Runtime Support

---

## 概述

RCoder 在 Kubernetes 环境中运行时，需要通过 kube-rs 库调用 K8s API Server 来动态创建和管理 Pod。

本文档提供完整的 RBAC 配置清单，确保 RCoder 有足够的权限来创建、查询和删除 Pod。

---

## 最小权限需求

| 资源 | 操作 | 用途 |
|------|------|------|
| pods | create | 创建 agent_runner Pod |
| pods | delete | 删除已完成的 Pod |
| pods | get | 查询 Pod 状态 |
| pods | list | 列出 Pod |
| pods | watch | 监听 Pod 状态变化 |
| pods/log | get | 查看 Pod 日志（可选） |

---

## 完整 RBAC 配置

### 方案一：ClusterRole（适用于所有 Namespace）

```yaml
# rcoder-rbac.yaml
---
# 1. ServiceAccount
apiVersion: v1
kind: ServiceAccount
metadata:
  name: rcoder-pods-sa
  # 如果 rcoder 运行在特定 namespace，修改这里
  # namespace: rcoder
---
# 2. ClusterRole（权限定义）
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: rcoder-pods-clusterrole
rules:
# Pod 管理
- apiGroups: [""]
  resources: ["pods"]
  verbs: ["create", "delete", "get", "list", "watch", "patch", "update"]
# Pod 日志
- apiGroups: [""]
  resources: ["pods/log"]
  verbs: ["get", "list"]
# Pod 执行命令（用于调试，可选）
- apiGroups: [""]
  resources: ["pods/exec"]
  verbs: ["create"]
# Pod 状态
- apiGroups: [""]
  resources: ["pods/status"]
  verbs: ["get"]
---
# 3. ClusterRoleBinding（绑定到 ServiceAccount）
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: rcoder-pods-clusterrolebinding
subjects:
# 如果在特定 namespace 使用 Role/RoleBinding，改为 kind: ServiceAccount
- kind: ServiceAccount
  name: rcoder-pods-sa
  # apiGroup 固定
  apiGroup: ""
roleRef:
  kind: ClusterRole
  name: rcoder-pods-clusterrole
  apiGroup: rbac.authorization.k8s.io
```

### 方案二：Role + RoleBinding（限定 Namespace）

适用于多租户环境，限制 RCoder 只能操作特定 namespace。

```yaml
# rcoder-rbac-namespaced.yaml
---
# 1. ServiceAccount
apiVersion: v1
kind: ServiceAccount
metadata:
  name: rcoder-pods-sa
  namespace: rcoder  # 指定 namespace
---
# 2. Role（权限定义）
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: rcoder-pods-role
  namespace: rcoder  # 指定 namespace
rules:
- apiGroups: [""]
  resources: ["pods"]
  verbs: ["create", "delete", "get", "list", "watch", "patch", "update"]
- apiGroups: [""]
  resources: ["pods/log"]
  verbs: ["get", "list"]
- apiGroups: [""]
  resources: ["pods/exec"]
  verbs: ["create"]
- apiGroups: [""]
  resources: ["pods/status"]
  verbs: ["get"]
---
# 3. RoleBinding（绑定到 ServiceAccount）
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: rcoder-pods-rolebinding
  namespace: rcoder  # 指定 namespace
subjects:
- kind: ServiceAccount
  name: rcoder-pods-sa
  namespace: rcoder
roleRef:
  kind: Role
  name: rcoder-pods-role
  apiGroup: rbac.authorization.k8s.io
```

---

## 部署步骤

### 1. 创建配置（使用 ClusterRole 方案）

```bash
# 应用 RBAC 配置
kubectl apply -f rcoder-rbac.yaml

# 验证 ServiceAccount 创建成功
kubectl get sa rcoder-pods-sa

# 验证 Role 创建成功
kubectl get clusterrole rcoder-pods-clusterrole

# 验证 RoleBinding 创建成功
kubectl get clusterrolebinding rcoder-pods-clusterrolebinding
```

### 2. 修改 RCoder Deployment

将 RCoder Pod 关联到 ServiceAccount：

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: rcoder
  namespace: default  # 或你的 namespace
spec:
  replicas: 1
  selector:
    matchLabels:
      app: rcoder
  template:
    metadata:
      labels:
        app: rcoder
    spec:
      # 添加 ServiceAccount 配置
      serviceAccountName: rcoder-pods-sa
      containers:
      - name: rcoder
        image: registry.yichamao.com/rcoder:latest
        ports:
        - containerPort: 8087
        env:
        # 启用 K8s 运行时
        - name: CONTAINER_RUNTIME
          value: "kubernetes"
        # 可选：指定 namespace（默认使用 default）
        - name: RCODER_K8S_NAMESPACE
          value: "default"
```

### 3. 验证权限

```bash
# 进入 RCoder Pod
kubectl exec -it <rcoder-pod-name> -- sh

# 测试 K8s API 访问权限
# 方法1：使用 kubectl（需要安装）
kubectl auth can-i create pods --as=system:serviceaccount:default:rcoder-pods-sa

# 方法2：直接测试 API
curl -k https://kubernetes.default.svc/api/v1/namespaces/default/pods \
  --header "Authorization: Bearer $(cat /var/run/secrets/kubernetes.io/serviceaccount/token)"
```

---

## 故障排查

### 问题 1: Permission Denied

```
Error: Container creation failed: Failed to create pod: Unauthorized
```

**解决**: 检查 ServiceAccount 是否正确绑定到 Role/ClusterRole

```bash
# 检查 ServiceAccount
kubectl get sa rcoder-pods-sa -o yaml

# 检查 RoleBinding subjects
kubectl get rolebinding <rolebinding-name> -o yaml
```

### 问题 2: Cannot create Pods in other namespaces

**原因**: 使用了 RoleBinding 而不是 ClusterRoleBinding

**解决**: 如果需要跨 namespace 操作 Pod，使用 ClusterRoleBinding

### 问题 3: Token 文件不存在

```
Error: K8s client init failed: No such file or directory: /var/run/secrets/kubernetes.io/serviceaccount/token
```

**原因**: 代码不在 Pod 内运行，或未配置 ServiceAccount

**解决**:
1. 确保 RCoder 部署在 K8s 集群内
2. 确保 Deployment 配置了 `serviceAccountName`
3. 检查是否挂载了 ServiceAccount token

```bash
# 检查 Pod 是否挂载了 token
kubectl exec <pod-name> -- ls -la /var/run/secrets/kubernetes.io/serviceaccount/
```

---

## 生产环境建议

### 1. 使用专用 namespace

```bash
# 创建独立 namespace
kubectl create namespace rcoder
```

### 2. 限制 Image Pull Secret（如果使用私有仓库）

```yaml
imagePullSecrets:
- name: regcred
```

### 3. NetworkPolicy（可选）

限制 Pod 网络通信：

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: rcoder-pods-networkpolicy
  namespace: rcoder
spec:
  podSelector:
    matchLabels:
      app: rcoder
  policyTypes:
  - Ingress
  - Egress
```

### 4. Resource Limits

为 RCoder Pod 设置资源限制：

```yaml
resources:
  requests:
    memory: "256Mi"
    cpu: "250m"
  limits:
    memory: "1Gi"
    cpu: "1000m"
```

---

## 代码集成

### 启动时权限检查（建议添加）

在 `KubernetesRuntime::new()` 中添加权限验证：

```rust
pub async fn new(config: DockerManagerConfig) -> ContainerRuntimeResult<Self> {
    let kube_config = Config::infer()
        .await
        .map_err(|e| ...)?;

    let client = Client::try_from(kube_config)
        .map_err(|e| ...)?;

    // 验证权限：尝试列出 pods
    let pods: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    pods.list(&ListParams::default().limit(1)).await
        .map_err(|e| ContainerRuntimeError::K8sError(
            format!("Permission denied or API server unreachable: {}", e)
        ))?;

    // ... 继续初始化
}
```

### 环境变量配置

| 环境变量 | 默认值 | 说明 |
|----------|--------|------|
| `CONTAINER_RUNTIME` | `docker` | 运行时类型：docker/kubernetes/k8s |
| `RCODER_K8S_NAMESPACE` | `default` | Pod 创建的 namespace |
| `RCODER_K8S_SERVICE_ACCOUNT` | `rcoder-pods-sa` | 使用的 ServiceAccount |

---

## 完整示例部署

参见下一节 "Helm Chart 或 Kustomize 清单"

---

## 相关文档

- [K8s RBAC 官方文档](https://kubernetes.io/docs/reference/access-authn-authz/rbac/)
- [kube-rs 认证文档](https://kube.rs/client/auth/)
- [ServiceAccount 配置](https://kubernetes.io/docs/tasks/configure-pod-container/configure-service-account/)

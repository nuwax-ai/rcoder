# Research: Kubernetes Runtime Support

**Date**: 2026-04-16  
**Feature**: K8s Runtime Support

## Research 1: K8s Service DNS vs Pod IP

### Decision
使用 K8s Service DNS 作为服务发现机制。

### Rationale
- **Pod IP 不稳定**: Pod 重启后 IP 会变化，直接使用 Pod IP 会导致 gRPC 通信失败
- **Service DNS 稳定**: 即使 Pod 重启，Service IP 保持不变（ClusterIP Service）
- **K8s 标准做法**: 社区推荐使用 Service 进行服务发现

### Alternatives Considered
| 方案 | 优点 | 缺点 |
|------|------|------|
| Pod IP | 简单直接 | Pod 重启后失效 |
| Headless Service | 可直接解析 Pod IP | 仍依赖 Pod DNS（不稳定） |
| Ingress | 支持外部访问 | 增加复杂度，不需要 |
| ExternalName Service | 简单 | 不适合内部服务 |

### Service DNS 格式
```
{service_name}.{namespace}.svc.cluster.local
```

对于 RCoder:
- RCoder Pod: `rcoder-agent-{project_id}.{namespace}.svc.cluster.local`
- ComputerAgent Pod: `computer-agent-runner-{user_id}.{namespace}.svc.cluster.local`

### Implementation
```rust
fn pod_dns_name(project_id: &str, user_id: Option<&str>, namespace: &str) -> String {
    let prefix = match user_id {
        Some(uid) => format!("computer-agent-runner-{}", uid),
        None => format!("rcoder-agent-{}", project_id),
    };
    format!("{}.{}.svc.cluster.local", prefix, namespace)
}
```

---

## Research 2: kube-rs 最佳实践

### kube-rs 版本
当前使用: `kube 0.98`

### 推荐的 API 使用模式

#### 1. Client 初始化
```rust
// 推荐：从 Config::infer() 自动检测 in-cluster vs local
let kube_config = Config::infer().await?;
let client = Client::try_from(kube_config)?;
```

#### 2. API 访问模式
```rust
// 推荐：使用 Api::namespaced() 访问 namespaced 资源
let pods: Api<Pod> = Api::namespaced(client, &namespace);

// 使用 ListParams 过滤
let lp = ListParams::default().labels(&format!("project_id={}", project_id));
let pods = pods.list(&lp).await?;
```

#### 3. 错误处理
```rust
match pods.get(&pod_name).await {
    Ok(pod) => { /* found */ }
    Err(kube::Error::Api(ae)) if ae.code == 404 => { /* not found */ }
    Err(e) => return Err(e),
}
```

---

## Research 3: K8s 健康检查 (Readiness Probe)

### 问题
Docker 模式使用 HTTP 轮询检查服务健康:
```rust
async fn wait_for_service_ready(service_url: &str) {
    loop {
        if http::get(service_url).is_ok() {
            return Ok(());
        }
        sleep().await;
    }
}
```

K8s 模式应该使用 K8s 原生的 Readiness Probe 概念。

### 分析
- **K8s Readiness Probe**: K8s 自动管理，决定 Pod 是否接收流量
- **当前实现**: 应用层轮询，与 K8s 概念不匹配

### 解决方案
仍然使用应用层健康检查（保持兼容性），但针对 K8s 环境优化:
- 使用 DNS 解析代替 IP
- 增加超时时间（K8s Pod 启动通常需要 30-60s）
- 复用容器运行时层的健康检查接口

### K8s Probe 配置（未来可能需要）
```yaml
readinessProbe:
  httpGet:
    path: /health
    port: 8086
  initialDelaySeconds: 10
  periodSeconds: 5
  timeoutSeconds: 3
  failureThreshold: 3
```

---

## Research 4: K8s 存储 (Workspace 处理)

### 问题
Docker 模式下使用 bind mount 共享文件系统:
```yaml
volumes:
  - /host/path:/container/path
```

K8s 中如何处理？

### 分析
| K8s 存储方案 | 适用场景 | 缺点 |
|--------------|----------|------|
| EmptyDir | 临时存储 | Pod 删除后数据丢失 |
| HostPath | 节点文件 | 安全性差，需要特权 |
| PVC | 持久存储 | 需要预先配置 StorageClass |
| NFS/CIFS | 共享存储 | 需要外部存储服务 |
| ConfigMap | 配置文件 | 不适合大文件 |

### RCoder 场景
RCoder 的 workspace 需要:
1. 持久化（用户代码、项目文件）
2. 跨容器共享（rcoder 主容器和 agent_runner 容器）

### 解决方案
- **开发环境**: 使用 PVC with ReadWriteMany (如果存储支持)
- **生产环境**: 使用 NFS 或云存储
- **简化方案**: 对于 K8s 支持，暂时不处理 workspace 挂载（使用容器内置存储）

### 实现计划
Phase 1 中暂时跳过 workspace 存储问题，标记为 [NEEDS CLARIFICATION: workspace 存储策略]

---

## Summary

### Key Decisions
1. **Service DNS**: 使用 `{prefix}-{id}.{namespace}.svc.cluster.local`
2. **kube-rs**: 使用 Api::namespaced() + ListParams 过滤
3. **健康检查**: 保持应用层检查，增加超时
4. **存储**: 标记为待解决问题

### Open Questions
1. [NEEDS CLARIFICATION]: K8s 环境中 rcoder 主服务如何与 agent_runner Pod 通信？(同 namespace 直连？)
2. [NEEDS CLARIFICATION]: workspace 存储使用 PVC 还是其他方案？
3. [NEEDS CLARIFICATION]: K8s Service Account 和 RBAC 权限如何预配置？

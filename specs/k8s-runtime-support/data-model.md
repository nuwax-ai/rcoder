# Data Model: Kubernetes Runtime Support

**Date**: 2026-04-16  
**Feature**: K8s Runtime Support

## 1. Interface Changes

### 1.1 ContainerRuntime Trait (container-runtime-api)

**Location**: `crates/container-runtime-api/src/runtime_trait.rs`

**Existing Methods**:
```rust
pub trait ContainerRuntime: Send + Sync {
    async fn create_container(&self, project_id, user_id, host_workspace_path, service_type, resource_limits) -> Result<ContainerBasicInfo>;
    async fn get_container_info(&self, project_id) -> Result<Option<ContainerBasicInfo>>;
    async fn find_container(&self, project_id, service_type) -> Result<Option<RuntimeContainerInfo>>;
    async fn stop_container(&self, project_id) -> Result<()>;
    async fn is_container_running(&self, project_id) -> Result<bool>;
    async fn list_containers(&self) -> Result<Vec<RuntimeContainerInfo>>;
    async fn cleanup_all(&self) -> Result<()>;
    async fn health_check(&self) -> Result<()>;
}
```

**New Methods**:
```rust
// 新增: 按标签列出容器
async fn list_containers_by_label(&self, label_selector: &str) -> Result<Vec<RuntimeContainerInfo>>;

// 新增: 获取 Pod 的 DNS 名称
fn get_service_dns_name(&self, project_id: &str, user_id: Option<&str>) -> String;
```

---

## 2. KubernetesRuntime Implementation

**Location**: `crates/docker_manager/src/runtime/kubernetes_runtime.rs`

### 2.1 Config Changes

```rust
#[derive(Debug, Clone)]
pub struct KubernetesRuntimeConfig {
    pub namespace: String,
    pub pod_ttl_seconds: Option<u64>,
    pub image_pull_secret: Option<String>,
    pub service_account_name: String,
    // 新增: DNS 域名后缀
    pub cluster_domain: String,  // 默认: "cluster.local"
}
```

### 2.2 Service DNS Name Generation

```rust
impl KubernetesRuntime {
    /// Generate stable service DNS name for a pod
    /// 
    /// Format: {prefix}-{id}.{namespace}.svc.{cluster_domain}
    /// Examples:
    ///   - RCoder: rcoder-agent-{project_id}.default.svc.cluster.local
    ///   - Computer: computer-agent-runner-{user_id}.default.svc.cluster.local
    pub fn get_service_dns_name(&self, project_id: &str, user_id: Option<&str>) -> String {
        let prefix = match user_id {
            Some(uid) => format!("computer-agent-runner-{}", uid),
            None => format!("rcoder-agent-{}", project_id),
        };
        format!(
            "{}.{}.svc.{}",
            prefix, self.namespace, self.config.cluster_domain
        )
    }
}
```

### 2.3 ContainerBasicInfo with Service URL

```rust
// KubernetesRuntime 返回的 ContainerBasicInfo.service_url 使用 DNS
ContainerBasicInfo {
    // ...
    container_ip: pod_ip,              // 仍保留 Pod IP（用于内部参考）
    service_url: format!(              // 使用稳定的 DNS
        "http://{}:{}",
        self.get_service_dns_name(project_id, user_id),
        shared_types::GRPC_DEFAULT_PORT
    ),
    // ...
}
```

### 2.4 user_id Support in create_container

```rust
async fn create_container(
    &self,
    project_id: Option<&str>,
    user_id: Option<&str>,  // ← 现在处理 user_id
    host_workspace_path: &str,
    service_type: ServiceType,
    resource_limits: Option<ServiceResourceLimits>,
) -> ContainerRuntimeResult<ContainerBasicInfo> {
    // 确定容器标识符
    let identifier = user_id.or(project_id).ok_or_else(|| {
        ContainerRuntimeError::ConfigurationError(
            "Either project_id or user_id must be provided".to_string()
        )
    })?;

    // 生成 Pod 名称时考虑 user_id
    let pod_name = match user_id {
        Some(uid) => format!("computer-agent-runner-{}", uid),
        None => format!("rcoder-agent-{}", project_id.unwrap()),
    };

    // ... 后续逻辑
}
```

### 2.5 list_containers_by_label Implementation

```rust
async fn list_containers_by_label(
    &self,
    label_selector: &str,
) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
    let lp = ListParams::default().labels(label_selector);
    let pods = self.pods().list(&lp).await?;
    
    let mut result = Vec::new();
    for p in pods.items {
        let pod: Pod = p;
        // ... 转换逻辑
        result.push(RuntimeContainerInfo { ... });
    }
    Ok(result)
}
```

---

## 3. Global Module Changes

**Location**: `crates/docker_manager/src/lib.rs`

### 3.1 New Initialization API

```rust
pub mod global {
    // 修改: 添加 runtime_type 参数
    pub async fn init_global_runtime(
        runtime_type: RuntimeType,
        config: DockerManagerConfig,
    ) -> DockerResult<()> {
        match runtime_type {
            RuntimeType::Kubernetes => {
                RuntimeManager::init(config).await?;
            }
            RuntimeType::Docker => {
                init_docker_manager_direct(config).await?;
            }
        }
    }

    // 新增: 获取运行时类型
    pub fn get_runtime_type() -> RuntimeType {
        RuntimeType::from_env()
    }
}
```

### 3.2 Backward Compatibility

```rust
// 保持向后兼容的初始化方法
pub async fn init_global_docker_manager_with_config(
    config: DockerManagerConfig,
) -> DockerResult<()> {
    // 自动检测运行时类型
    init_global_runtime(RuntimeType::from_env(), config).await
}
```

---

## 4. Error Types

**Location**: `crates/container-runtime-api/src/runtime_trait.rs`

```rust
#[derive(Error, Debug)]
pub enum ContainerRuntimeError {
    #[error("Connection error: {0}")]
    ConnectionError(String),

    #[error("Container creation failed: {0}")]
    ContainerCreationError(String),

    // ... existing errors ...

    // 新增 K8s 相关错误
    #[error("Kubernetes error: {0}")]
    K8sError(String),

    #[error("Pod not found: {0}")]
    PodNotFound(String),

    #[error("Service DNS resolution failed: {0}")]
    DnsResolutionError(String),
}
```

---

## 5. Entity Relationships

```
┌─────────────────────────────────────────────────────────────┐
│                      RuntimeManager                          │
│  - select_runtime() -> Arc<dyn ContainerRuntime>            │
└─────────────────────────┬───────────────────────────────────┘
                          │
          ┌───────────────┴───────────────┐
          │                               │
          ▼                               ▼
┌─────────────────┐           ┌─────────────────────┐
│  DockerRuntime  │           │ KubernetesRuntime   │
│  (wrapper)      │           │                     │
├─────────────────┤           ├─────────────────────┤
│ - inner:        │           │ - client: Client    │
│   DockerManager │           │ - namespace: String  │
├─────────────────┤           │ - config: K8sConfig │
│ Implements:     │           ├─────────────────────┤
│ ContainerRuntime           │ Implements:         │
└─────────────────┘           │ ContainerRuntime    │
                              │ + Service DNS      │
                              └─────────────────────┘
```

---

## 6. State Transitions

### Container State (K8s)

```
                    ┌──────────┐
                    │ Pending  │
                    └─────┬────┘
                          │ (Pod scheduled)
                          ▼
                    ┌──────────┐
        ┌──────────│ Running  │──────────┐
        │          └────┬─────┘          │
        │               │ (container     │
        │               │ exits)        │
        ▼               ▼                ▼
   ┌─────────┐    ┌──────────┐    ┌──────────┐
   │ Succeeded│    │ Failed   │    │ Unknown  │
   └─────────┘    └──────────┘    └──────────┘
```

### RCoder Container Status Mapping

| K8s Phase | RCoder Status |
|-----------|---------------|
| Pending | Creating |
| Running | Running |
| Succeeded | Stopped |
| Failed | Failed |
| Unknown | Unknown |

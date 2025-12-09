# RCoder 双 Docker 镜像配置技术设计文档

## 📋 概述

本文档描述了 RCoder 系统中双 Docker 镜像配置的设计方案，支持在动态创建容器时指定 RCoder 或 AgentRunner 服务类型，以支持当前功能和未来新功能的开发。

### 背景

当前 RCoder 系统使用 `registry.yichamao.com/rcoder` 镜像。为了支持新功能开发，需要引入 `registry.yichamao.com/rcoder-agent-runner` 镜像，同时保持现有功能的稳定性。需要设计一个简化的双镜像配置系统。

### 目标

1. **支持两种服务类型**: rcoder (当前使用) 和 agent-runner (新功能)
2. **保持架构兼容性**: 支持 ARM64/AMD64 多架构
3. **灵活配置**: 支持默认镜像、服务特定镜像、项目级镜像覆盖
4. **向后兼容**: 不破坏现有配置和功能，默认使用 rcoder 服务
5. **简化实现**: 降低复杂度，便于维护和扩展

## 🏗️ 整体架构设计

### 配置层级结构

```
全局默认镜像配置
    ↓
服务类型特定配置 (rcoder/agent-runner/specialized-tools)
    ↓
项目级镜像覆盖 (可选)
    ↓
运行时镜像选择 (最终使用的镜像)
```

### 核心组件

1. **ServiceType**: 服务类型枚举
2. **MultiImageConfig**: 多镜像配置结构
3. **ImageSelector**: 镜像选择器
4. **ImageRegistry**: 镜像注册表

## 📐 详细设计

### 1. 服务类型定义

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ServiceType {
    /// 标准的 rcoder 服务 (当前默认使用的服务)
    RCoder,
    /// Agent Runner 服务 (新功能服务，后续开发使用)
    AgentRunner,
}

impl ServiceType {
    pub fn as_str(&self) -> &str {
        match self {
            ServiceType::RCoder => "rcoder",
            ServiceType::AgentRunner => "agent-runner",
        }
    }
    
    pub fn from_str(s: &str) -> Self {
        match s {
            "rcoder" => ServiceType::RCoder,
            "agent-runner" => ServiceType::AgentRunner,
            _ => {
                tracing::warn!("未知的服务类型 '{}'，使用默认的 RCoder 服务", s);
                ServiceType::RCoder
            }
        }
    }
    
    /// 获取服务描述
    pub fn description(&self) -> &str {
        match self {
            ServiceType::RCoder => "标准 RCoder 服务，提供完整的 AI 开发功能",
            ServiceType::AgentRunner => "Agent Runner 服务，专注于代理运行和执行",
        }
    }
}

// 注意：移除了 Default trait，强制要求明确指定服务类型
```

### 2. 服务镜像配置

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceImageConfig {
    /// 服务类型
    pub service_type: ServiceType,
    /// 通用镜像（优先级最高）
    pub image: Option<String>,
    /// ARM64 架构专用镜像
    pub arm64_image: Option<String>,
    /// AMD64 架构专用镜像
    pub amd64_image: Option<String>,
    /// 默认回退镜像
    pub default_image: Option<String>,
    /// 镜像标签前缀（用于自动构建镜像名称）
    pub image_tag_prefix: Option<String>,
    /// 是否启用该服务类型
    pub enabled: bool,
    /// 服务特定的环境变量
    pub environment: HashMap<String, String>,
    /// 服务特定的挂载点
    pub mounts: Vec<ServiceMountConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceMountConfig {
    /// 容器内路径
    pub container_path: String,
    /// 宿主机路径（支持变量替换）
    pub host_path: String,
    /// 是否只读
    pub read_only: bool,
    /// 挂载类型
    pub mount_type: String, // "bind", "volume"
}
```

### 3. 多镜像配置结构

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiImageConfig {
    /// 全局默认镜像配置
    pub global_defaults: GlobalImageDefaults,
    /// 各服务类型的镜像配置
    pub services: HashMap<String, ServiceImageConfig>,
    /// 镜像选择策略
    pub selection_strategy: ImageSelectionStrategy,
    /// 镜像缓存配置
    pub cache_config: ImageCacheConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalImageDefaults {
    /// 通用默认镜像
    pub image: Option<String>,
    /// 默认 ARM64 镜像
    pub arm64_image: Option<String>,
    /// 默认 AMD64 镜像
    pub amd64_image: Option<String>,
    /// 默认回退镜像
    pub default_image: Option<String>,
    /// 镜像仓库前缀
    pub registry_prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ImageSelectionStrategy {
    /// 仅使用服务特定配置（强制明确指定服务类型）
    ServiceOnly,
}

impl Default for ImageSelectionStrategy {
    fn default() -> Self {
        ImageSelectionStrategy::ServiceOnly
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageCacheConfig {
    /// 是否启用镜像缓存
    pub enabled: bool,
    /// 缓存过期时间（秒）
    pub ttl_seconds: u64,
    /// 最大缓存条目数
    pub max_entries: usize,
}
```

### 4. 配置文件结构

```yaml
# Docker 双镜像配置 - 简化版本（仅支持 RCoder 和 AgentRunner）
# 注意：创建容器时必须明确指定服务类型，不允许默认值
docker_config:
  # 全局默认配置
  global_defaults:
    # 默认镜像仓库前缀
    registry_prefix: "registry.yichamao.com"
    
    # 全局默认镜像配置
    image: null  # 留空使用服务特定配置
    arm64_image: "registry.yichamao.com/default:latest-arm64"
    amd64_image: "registry.yichamao.com/default:latest-amd64"
    default_image: "registry.yichamao.com/default:latest"
  
  # 镜像选择策略
  selection_strategy: "ServiceOnly"  # 仅使用服务特定配置，强制明确指定
  
  # 各服务类型的配置
  services:
    # 标准 RCoder 服务配置 (当前项目使用)
    rcoder:
      service_type: "rcoder"
      image: null  # 使用架构特定镜像
      arm64_image: "registry.yichamao.com/rcoder:latest-arm64"
      amd64_image: "registry.yichamao.com/rcoder:latest-amd64"
      default_image: "registry.yichamao.com/rcoder:latest"
      image_tag_prefix: "rcoder"
      enabled: true  # 当前启用
      environment:
        RUST_LOG: "info"
        SERVICE_MODE: "full"
        API_PORT: "8086"
      mounts:
        - container_path: "/app/project_workspace"
          host_path: "./project_workspace"
          read_only: false
          mount_type: "bind"
    
    # Agent Runner 服务配置 (新功能，后续开发使用)
    agent-runner:
      service_type: "agent-runner"
      image: null  # 使用架构特定镜像
      arm64_image: "registry.yichamao.com/rcoder-agent-runner:latest-arm64"
      amd64_image: "registry.yichamao.com/rcoder-agent-runner:latest-amd64"
      default_image: "registry.yichamao.com/rcoder-agent-runner:latest"
      image_tag_prefix: "rcoder-agent-runner"
      enabled: false  # 默认禁用，等待新功能开发
      environment:
        RUST_LOG: "debug"
        SERVICE_MODE: "agent-only"
        AGENT_PORT: "8086"
      mounts:
        - container_path: "/app/workspace"
          host_path: "./project_workspace/{project_id}"
          read_only: false
          mount_type: "bind"
        - container_path: "/app/models"
          host_path: "./models"
          read_only: true
          mount_type: "bind"
  
  # 镜像缓存配置
  cache_config:
    enabled: true
    ttl_seconds: 3600  # 1小时
    max_entries: 50     # 减少缓存条目数，因为只有2个服务
  
  # 其他现有配置保持不变
  network_mode: "bridge"
  work_dir: "/app"
  auto_cleanup: true
  container_ttl_seconds: 3600
```

### 5. 镜像选择器实现

```rust
pub struct ImageSelector {
    config: MultiImageConfig,
    cache: Arc<RwLock<HashMap<String, CachedImageInfo>>>,
    platform: String,
}

#[derive(Debug, Clone)]
pub struct CachedImageInfo {
    pub image_name: String,
    pub service_type: ServiceType,
    pub platform: String,
    pub cached_at: std::time::SystemTime,
}

impl ImageSelector {
    pub fn new(config: MultiImageConfig) -> Self {
        let platform = crate::utils::DockerUtils::get_optimal_platform();
        Self {
            config,
            cache: Arc::new(RwLock::new(HashMap::new())),
            platform,
        }
    }
    
    /// 根据服务类型和项目配置选择镜像
    /// 注意：service_type 不能为空，必须明确指定
    pub fn select_image(
        &self,
        service_type: &ServiceType,
        project_overrides: Option<&ProjectImageOverrides>,
    ) -> DockerResult<String> {
        // 强制验证：service_type 必须明确指定
        if !self.is_service_enabled(service_type) {
            return Err(DockerError::ConfigurationError(
                format!("服务类型 '{}' 未启用或配置不存在", service_type.as_str())
            ));
        }
        
        let cache_key = self.build_cache_key(service_type, project_overrides);
        
        // 检查缓存
        if let Some(cached) = self.get_from_cache(&cache_key) {
            return Ok(cached.image_name);
        }
        
        // 强制使用 ServiceOnly 策略：仅使用服务特定配置
        let image_name = self.select_service_only(service_type, project_overrides)?;
        
        // 缓存结果
        self.cache_image_info(&cache_key, &image_name, service_type);
        
        Ok(image_name)
    }
    
    /// 检查服务是否已启用和配置
    fn is_service_enabled(&self, service_type: &ServiceType) -> bool {
        let service_key = service_type.as_str();
        if let Some(service_config) = self.config.services.get(service_key) {
            service_config.enabled
        } else {
            false
        }
    }
    
    fn select_service_first(
        &self,
        service_type: &ServiceType,
        project_overrides: Option<&ProjectImageOverrides>,
    ) -> DockerResult<String> {
        let service_key = service_type.as_str();
        
        // 1. 检查项目级覆盖
        if let Some(overrides) = project_overrides {
            if let Some(project_image) = self.get_project_override_image(overrides, service_type) {
                return Ok(project_image);
            }
        }
        
        // 2. 检查服务特定配置
        if let Some(service_config) = self.config.services.get(service_key) {
            if !service_config.enabled {
                return Err(DockerError::ConfigurationError(
                    format!("服务类型 {} 未启用", service_key)
                ));
            }
            
            if let Some(image) = self.select_service_image(service_config) {
                return Ok(image);
            }
        }
        
        // 3. 回退到全局默认配置
        self.select_global_default_image()
    }
    
    fn select_service_image(&self, config: &ServiceImageConfig) -> Option<String> {
        // 1. 优先使用通用镜像
        if let Some(image) = &config.image {
            return Some(image.clone());
        }
        
        // 2. 根据架构选择特定镜像
        match self.platform.as_str() {
            "linux/arm64" => {
                config.arm64_image
                    .clone()
                    .or_else(|| config.default_image.clone())
            }
            "linux/amd64" => {
                config.amd64_image
                    .clone()
                    .or_else(|| config.default_image.clone())
            }
            _ => config.default_image.clone(),
        }
    }
    
    fn select_global_default_image(&self) -> DockerResult<String> {
        let defaults = &self.config.global_defaults;
        
        // 1. 优先使用全局通用镜像
        if let Some(image) = &defaults.image {
            return Ok(image.clone());
        }
        
        // 2. 根据架构选择全局默认镜像
        let image = match self.platform.as_str() {
            "linux/arm64" => {
                defaults.arm64_image
                    .clone()
                    .or_else(|| defaults.default_image.clone())
            }
            "linux/amd64" => {
                defaults.amd64_image
                    .clone()
                    .or_else(|| defaults.default_image.clone())
            }
            _ => defaults.default_image.clone(),
        };
        
        image.ok_or_else(|| {
            DockerError::ConfigurationError(
                "无法找到适合的默认镜像配置".to_string()
            )
        })
    }
    
    fn build_cache_key(
        &self,
        service_type: &ServiceType,
        project_overrides: Option<&ProjectImageOverrides>,
    ) -> String {
        let base_key = format!("{}:{}", service_type.as_str(), self.platform);
        
        if let Some(overrides) = project_overrides {
            format!("{}:overrides:{}", base_key, overrides.hash_key())
        } else {
            base_key
        }
    }
}

/// 项目级镜像覆盖配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectImageOverrides {
    /// 项目特定的镜像配置
    pub images: HashMap<String, String>,
    /// 启用的服务类型
    pub enabled_services: Vec<String>,
    /// 项目特定的环境变量
    pub environment: HashMap<String, String>,
}

impl ProjectImageOverrides {
    pub fn hash_key(&self) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        let mut hasher = DefaultHasher::new();
        
        // 哈希镜像配置
        for (key, value) in &self.images {
            key.hash(&mut hasher);
            value.hash(&mut hasher);
        }
        
        // 哈希启用的服务
        for service in &self.enabled_services {
            service.hash(&mut hasher);
        }
        
        format!("{:x}", hasher.finish())
    }
}
```

## 🔧 实现建议

### 1. 配置文件兼容性

为保持向后兼容，支持两种配置方式：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerConfig {
    /// 新的多镜像配置（优先）
    #[serde(default)]
    pub multi_image_config: Option<MultiImageConfig>,
    
    /// 传统单一镜像配置（向后兼容）
    pub image: Option<String>,
    pub arm64_image: Option<String>,
    pub amd64_image: Option<String>,
    pub default_image: Option<String>,
    
    // ... 其他现有字段
}

impl DockerConfig {
    /// 获取多镜像配置，如果未配置则使用传统配置创建默认配置
    pub fn get_multi_image_config(&self) -> MultiImageConfig {
        if let Some(ref config) = self.multi_image_config {
            config.clone()
        } else {
            // 从传统配置创建默认多镜像配置
            self.create_legacy_multi_config()
        }
    }
    
    fn create_legacy_multi_config(&self) -> MultiImageConfig {
        MultiImageConfig {
            default_service_type: ServiceType::RCoder,
            global_defaults: GlobalImageDefaults {
                image: self.image.clone(),
                arm64_image: self.arm64_image.clone(),
                amd64_image: self.amd64_image.clone(),
                default_image: self.default_image.clone(),
                registry_prefix: Some("registry.yichamao.com".to_string()),
            },
            services: {
                let mut services = HashMap::new();
                services.insert(
                    "rcoder".to_string(),
                    ServiceImageConfig {
                        service_type: ServiceType::RCoder,
                        image: self.image.clone(),
                        arm64_image: self.arm64_image.clone(),
                        amd64_image: self.amd64_image.clone(),
                        default_image: self.default_image.clone(),
                        image_tag_prefix: Some("rcoder".to_string()),
                        enabled: true,
                        environment: HashMap::new(),
                        mounts: Vec::new(),
                    },
                );
                services
            },
            selection_strategy: ImageSelectionStrategy::ServiceFirst,
            cache_config: ImageCacheConfig {
                enabled: true,
                ttl_seconds: 3600,
                max_entries: 100,
            },
        }
    }
}
```

### 2. 容器创建接口更新

```rust
impl DockerManager {
    /// 使用多镜像配置创建容器
    pub async fn create_container_with_service_type(
        &self,
        mut config: DockerContainerConfig,
        service_type: ServiceType,
        project_overrides: Option<ProjectImageOverrides>,
    ) -> DockerResult<DockerContainerInfo> {
        // 选择合适的镜像
        let image_selector = ImageSelector::new(self.get_multi_image_config());
        let selected_image = image_selector.select_image(&service_type, project_overrides.as_ref())?;
        
        // 更新容器配置
        config.image = selected_image;
        
        // 添加服务特定的环境变量
        if let Some(service_config) = image_selector.get_service_config(&service_type) {
            for (key, value) in &service_config.environment {
                config.env_vars.insert(key.clone(), value.clone());
            }
            
            // 添加服务特定的挂载点
            for mount_config in &service_config.mounts {
                let host_path = self.resolve_host_path(&mount_config.host_path, &config.project_id)?;
                config.extra_mounts.push(ExtraMount {
                    host_path,
                    container_path: mount_config.container_path.clone(),
                    read_only: mount_config.read_only,
                });
            }
        }
        
        // 使用现有逻辑创建容器
        self.create_container(config).await
    }
    
    /// 获取多镜像配置
    fn get_multi_image_config(&self) -> MultiImageConfig {
        self.config.get_multi_image_config()
    }
    
    /// 解析宿主机路径（支持变量替换）
    fn resolve_host_path(&self, path_template: &str, project_id: &str) -> DockerResult<String> {
        let resolved = path_template
            .replace("{project_id}", project_id)
            .replace("{workspace_dir}", &self.config.default_work_dir);
        
        Ok(resolved)
    }
}
```

### 3. 项目级配置支持

支持在项目目录中创建 `.rcoder-image.yml` 文件来覆盖镜像配置：

```yaml
# .rcoder-image.yml (项目级配置)
project_id: "my-special-project"
service_type: "agent-runner"  # 可选择使用 AgentRunner 服务

# 项目特定的镜像覆盖
images:
  rcoder: "my-custom-registry/rcoder:custom-v1.0"
  agent-runner: "my-custom-registry/agent-runner:custom-v1.0"

# 启用的服务类型
enabled_services:
  - "rcoder"
  - "agent-runner"

# 项目特定的环境变量
environment:
  AGENT_MODE: "production"
  LOG_LEVEL: "debug"
  RCODER_FEATURES: "full"
```

### 4. API 接口扩展

扩展现有的聊天接口支持服务类型选择：

```rust
#[derive(Debug, Deserialize, Serialize)]
pub struct ChatRequest {
    pub prompt: String,
    pub project_id: Option<String>,
    pub session_id: Option<String>,
    pub attachments: Vec<Attachment>,
    
    // 必填：服务类型选择 (强制要求指定)
    pub service_type: String,  // "rcoder" 或 "agent-runner"，不允许为空
    
    // 可选：项目级镜像覆盖
    pub image_overrides: Option<HashMap<String, String>>,
    
    // 现有字段...
    pub model_provider: Option<ModelProviderConfig>,
    pub request_id: Option<String>,
}
```

## 📋 实施计划

### 阶段 1: 基础结构（1 天）
1. 定义简化的 ServiceType (RCoder, AgentRunner)
2. 实现基础的 MultiImageConfig
3. 更新配置文件解析逻辑

### 阶段 2: 镜像选择器（1-2 天）
1. 实现简化的 ImageSelector 核心逻辑
2. 添加镜像缓存机制
3. 实现向后兼容性支持

### 阶段 3: 容器创建集成（1-2 天）
1. 更新 DockerManager 接口
2. 集成服务类型到容器创建流程
3. 实现项目级配置支持

### 阶段 4: API 和测试（1-2 天）
1. 扩展 API 接口支持服务类型选择
2. 更新文档和示例
3. 编写核心测试用例

### 阶段 5: 部署和监控（1 天）
1. 更新部署脚本和文档
2. 添加基础监控和日志
3. 简化性能测试

## 🔍 测试策略

### 单元测试
- 镜像选择逻辑测试
- 配置解析测试
- 缓存机制测试

### 集成测试
- 容器创建流程测试
- 多服务类型协同测试
- 项目级配置覆盖测试

### 端到端测试
- 完整的聊天流程测试
- 不同服务类型的容器启动测试
- 配置热更新测试

## 📊 性能考虑

### 镜像缓存
- 内存缓存减少重复计算
- TTL 机制避免过期配置
- LRU 策略控制内存使用

### 并发安全
- DashMap 用于线程安全的缓存访问
- 读写锁保护配置更新
- 原子操作确保一致性

## 🔒 安全考虑

### 配置验证
- 镜像名称格式验证
- 路径注入防护
- 环境变量安全检查

### 访问控制
- 项目级配置权限控制
- 服务类型启用/禁用控制
- 镜像仓库访问权限

## 📈 监控和日志

### 指标收集
- 镜像选择延迟
- 缓存命中率
- RCoder vs AgentRunner 使用统计

### 日志记录
- 镜像选择决策日志
- 配置解析错误日志
- 容器创建失败日志

---

*本文档版本: v1.0*  
*最后更新: 2025-12-02*
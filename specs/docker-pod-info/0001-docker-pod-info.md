# Docker Pod 信息接口设计文档

> **文档版本**: v1.0.1  
> **创建日期**: 2024-12-16  
> **更新日期**: 2024-12-16  
> **状态**: 草案  
> **相关模块**: `crates/rcoder/src/router.rs`, `crates/docker_manager`, `crates/shared_types`

---

## 1. 需求概述

### 1.1 背景

RCoder 系统采用动态容器管理模式，可以根据 `user_id` 和 `project_id` 创建和管理 Docker 容器（特别是 `ComputerAgentRunner` 类型的容器）。为了便于系统监控和前端用户使用 noVNC 远程虚拟桌面，需要新增三个接口：

1. **获取当前容器数量接口** - 用于监控系统中已创建的容器总数
2. **启动容器接口** - 根据 `user_id` 和 `project_id` 启动（或获取已存在的）容器，**仅启动容器本身，不启动 Agent 服务**，以便用户可以通过 noVNC 访问远程虚拟桌面
3. **容器保活接口** - 刷新容器的最后活动时间，防止容器被定时清理任务销毁

> [!IMPORTANT]
> 所有接口响应统一使用 `shared_types::HttpResult<T>` 结构进行包装，保持与系统其他 API 的一致性。

### 1.2 目标

| 目标 | 描述 |
|------|------|
| **监控能力** | 提供容器数量统计，便于系统运维监控 |
| **用户体验** | 简化容器启动流程，自动化处理容器存在性检查 |
| **简洁响应** | 响应只包含容器基本信息，VNC 访问通过独立接口获取 |
| **轻量启动** | 仅启动容器，不启动 Agent 服务，减少资源占用 |
| **容器保活** | 支持刷新容器活动时间，防止被自动清理 |

---

## 2. HttpResult 响应包装

### 2.1 HttpResult 结构 (来自 `shared_types`)

所有接口响应均使用 `HttpResult<T>` 包装：

```rust
// 来自 crates/shared_types/src/model/http_result.rs

#[derive(Debug, Deserialize, ToSchema)]
pub struct HttpResult<T> {
    /// 响应码: "0000" 表示成功
    pub code: String,
    
    /// 响应消息
    pub message: String,
    
    /// 响应数据 (成功时有值)
    pub data: Option<T>,
    
    /// OpenTelemetry Trace ID (用于链路追踪)
    pub tid: Option<String>,
    
    /// 是否成功 (code == "0000")
    #[serde(skip)]
    pub success: bool,
}

impl<T> HttpResult<T> {
    /// 创建成功响应
    pub fn success(data: T) -> Self;
    
    /// 创建错误响应
    pub fn error(code: &str, message: &str) -> Self;
    
    /// 创建内部错误响应 (code: "5000")
    pub fn internal_error(message: &str) -> Self;
}
```

### 2.2 响应格式示例

**成功响应:**

```json
{
  "code": "0000",
  "message": "成功",
  "data": { ... },
  "tid": "abc123def456...",
  "success": true
}
```

**错误响应:**

```json
{
  "code": "5000",
  "message": "获取 DockerManager 失败",
  "data": null,
  "tid": "abc123def456...",
  "success": false
}
```

---

## 3. 接口设计

### 3.1 接口一：获取当前容器数量

#### 3.1.1 接口规格

| 属性 | 值 |
|------|-----|
| **路径** | `GET /computer/pod/count` |
| **方法** | GET |
| **入参** | 无 |
| **响应类型** | `HttpResult<PodCountResponse>` |
| **标签** | `pod` (Pod 容器管理接口) |

#### 3.1.2 请求示例

```bash
curl -X GET http://localhost:8087/computer/pod/count
```

#### 3.1.3 响应数据结构 (data 字段)

```rust
/// 容器数量响应结构
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PodCountResponse {
    /// 当前运行的容器总数
    #[schema(example = 5)]
    pub total_count: u32,
    
    /// 按服务类型分类的容器数量
    pub by_service_type: PodCountByServiceType,
    
    /// 统计时间戳 (Unix 毫秒)
    #[schema(example = 1702700000000)]
    pub timestamp: u64,
}

/// 按服务类型分类的容器数量
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PodCountByServiceType {
    /// RCoder 类型容器数量
    #[schema(example = 2)]
    pub rcoder: u32,
    
    /// ComputerAgentRunner 类型容器数量
    #[schema(example = 3)]
    pub computer_agent_runner: u32,
}
```

#### 3.1.4 完整响应示例

```json
{
  "code": "0000",
  "message": "成功",
  "data": {
    "total_count": 5,
    "by_service_type": {
      "rcoder": 2,
      "computer_agent_runner": 3
    },
    "timestamp": 1702700000000
  },
  "tid": "4bf92f3577b34da6a3ce929d0e0e4736",
  "success": true
}
```

---

### 3.2 接口二：启动容器 (Ensure Container)

#### 3.2.1 接口规格

| 属性 | 值 |
|------|-----|
| **路径** | `POST /computer/pod/ensure` |
| **方法** | POST |
| **入参** | `user_id` (必填), `project_id` (必填) |
| **响应类型** | `HttpResult<EnsurePodResponse>` |
| **标签** | `pod` (Pod 容器管理接口) |

> [!NOTE]
> 此接口采用 **幂等设计**：如果对应的容器已经存在且处于运行状态，则直接返回现有容器信息，不会创建新容器。

> [!IMPORTANT]
> **此接口仅启动容器，不启动 Agent 服务。** 容器启动后，用户可直接通过 noVNC 访问虚拟桌面，无需等待 Agent 服务初始化。如需使用 AI Agent 功能，请调用 `/computer/chat` 接口。

#### 3.2.2 请求结构

```rust
/// 启动容器请求结构
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct EnsurePodRequest {
    /// 用户唯一标识符
    #[schema(example = "user_123")]
    pub user_id: String,
    
    /// 项目唯一标识符
    #[schema(example = "proj_456")]
    pub project_id: String,
    
    /// 可选的资源限制配置
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_limits: Option<PodResourceLimits>,
}

/// Pod 资源限制配置
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct PodResourceLimits {
    /// 内存限制 (bytes), 例如 4GB = 4294967296
    #[schema(example = 4294967296)]
    pub memory: Option<u64>,
    
    /// CPU 份额 (1024 = 1 核)
    #[schema(example = 2048)]
    pub cpu_shares: Option<u64>,
}
```

#### 3.2.3 请求示例

```bash
curl -X POST http://localhost:8087/computer/pod/ensure \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "user_123",
    "project_id": "proj_456"
  }'
```

#### 3.2.4 响应数据结构 (data 字段)

```rust
/// 启动容器响应结构
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EnsurePodResponse {
    /// 容器是否为新创建 (false 表示已存在)
    pub created: bool,

    /// 容器基本信息
    pub container_info: PodContainerInfo,

    /// 提示消息
    #[schema(example = "容器已就绪")]
    pub message: String,
}

/// 容器基本信息
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PodContainerInfo {
    /// 容器 ID
    #[schema(example = "abc123def456")]
    pub container_id: String,

    /// 容器名称
    #[schema(example = "computer-agent-runner-user_123")]
    pub container_name: String,

    /// 容器 IP 地址 (内部网络)
    #[schema(example = "172.17.0.5")]
    pub container_ip: String,

    /// 服务 URL
    #[schema(example = "http://172.17.0.5:8086")]
    pub service_url: String,

    /// 容器状态
    #[schema(example = "running")]
    pub status: String,
}
```

#### 3.2.5 完整响应示例

**成功响应（新创建容器）:**

```json
{
  "code": "0000",
  "message": "成功",
  "data": {
    "created": true,
    "container_info": {
      "container_id": "abc123def456...",
      "container_name": "computer-agent-runner-user_123",
      "container_ip": "172.17.0.5",
      "service_url": "http://172.17.0.5:8086",
      "status": "running"
    },
    "message": "容器创建成功（Agent 服务未启动）"
  },
  "tid": "4bf92f3577b34da6a3ce929d0e0e4736",
  "success": true
}
```

**成功响应（容器已存在）:**

```json
{
  "code": "0000",
  "message": "成功",
  "data": {
    "created": false,
    "container_info": {
      "container_id": "abc123def456...",
      "container_name": "computer-agent-runner-user_123",
      "container_ip": "172.17.0.5",
      "service_url": "http://172.17.0.5:8086",
      "status": "running"
    },
    "message": "容器已存在"
  },
  "tid": "4bf92f3577b34da6a3ce929d0e0e4736",
  "success": true
}
```

**错误响应:**

```json
{
  "code": "5000",
  "message": "启动容器失败: Docker 连接超时",
  "data": null,
  "tid": "4bf92f3577b34da6a3ce929d0e0e4736",
  "success": false
}
```

---

### 3.3 接口三：容器保活 (Keepalive)

#### 3.3.1 接口规格

| 属性 | 值 |
|------|-----|
| **路径** | `POST /computer/pod/keepalive` |
| **方法** | POST |
| **入参** | `user_id` (必填), `project_id` (必填) |
| **响应类型** | `HttpResult<KeepalivePodResponse>` |
| **标签** | `pod` (Pod 容器管理接口) |

> [!NOTE]
> 此接口用于防止容器被定时清理任务销毁。系统会定期（默认每 5 分钟）检查容器的最后活动时间，闲置超过 30 分钟的容器将被自动清理。

> [!TIP]
> **使用场景**：前端在用户使用 noVNC 远程桌面时，应定期调用此接口（建议每 5-10 分钟一次）来保持容器活跃状态。

#### 3.3.2 请求结构

```rust
/// 容器保活请求结构
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct KeepalivePodRequest {
    /// 用户唯一标识符
    #[schema(example = "user_123")]
    pub user_id: String,
    
    /// 项目唯一标识符
    #[schema(example = "proj_456")]
    pub project_id: String,
}
```

#### 3.3.3 请求示例

```bash
curl -X POST http://localhost:8087/computer/pod/keepalive \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "user_123",
    "project_id": "proj_456"
  }'
```

#### 3.3.4 响应数据结构 (data 字段)

```rust
/// 容器保活响应结构
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct KeepalivePodResponse {
    /// 容器是否存在 (true=已存在, false=新创建)
    pub existed: bool,
    
    /// 容器是否为新创建 (当 existed=false 时为 true)
    pub created: bool,
    
    /// 容器基本信息
    pub container_info: PodContainerInfo,
    
    /// 上次活动时间 (Unix 毫秒时间戳, 更新前)
    #[schema(example = 1702700000000)]
    pub previous_activity_time: u64,
    
    /// 当前活动时间 (Unix 毫秒时间戳, 更新后)
    #[schema(example = 1702700600000)]
    pub current_activity_time: u64,
    
    /// 距离下次清理的剩余时间 (秒)
    /// 基于默认 30 分钟闲置超时计算
    #[schema(example = 1800)]
    pub time_until_cleanup: u64,
    
    /// 提示消息
    #[schema(example = "容器活动时间已刷新")]
    pub message: String,
}
```

#### 3.3.5 完整响应示例

**成功响应（容器已存在，刷新活动时间）:**

```json
{
  "code": "0000",
  "message": "成功",
  "data": {
    "existed": true,
    "created": false,
    "container_info": {
      "container_id": "abc123def456...",
      "container_name": "computer-agent-runner-user_123",
      "container_ip": "172.17.0.5",
      "service_url": "http://172.17.0.5:8086",
      "status": "running"
    },
    "previous_activity_time": 1702700000000,
    "current_activity_time": 1702700600000,
    "time_until_cleanup": 1800,
    "message": "容器活动时间已刷新，距离自动清理还有 30 分钟"
  },
  "tid": "4bf92f3577b34da6a3ce929d0e0e4736",
  "success": true
}
```

**成功响应（容器不存在，自动创建）:**

```json
{
  "code": "0000",
  "message": "成功",
  "data": {
    "existed": false,
    "created": true,
    "container_info": {
      "container_id": "xyz789...",
      "container_name": "computer-agent-runner-user_123",
      "container_ip": "172.17.0.6",
      "service_url": "http://172.17.0.6:8086",
      "status": "running"
    },
    "previous_activity_time": 0,
    "current_activity_time": 1702700600000,
    "time_until_cleanup": 1800,
    "message": "容器已自动创建，距离自动清理还有 30 分钟"
  },
  "tid": "4bf92f3577b34da6a3ce929d0e0e4736",
  "success": true
}
```

**错误响应:**

```json
{
  "code": "5000",
  "message": "刷新活动时间失败: 容器状态异常",
  "data": null,
  "tid": "4bf92f3577b34da6a3ce929d0e0e4736",
  "success": false
}
```

---

## 4. 架构设计

### 4.1 整体架构

```
┌─────────────────────────────────────────────────────────────────┐
│                         外部客户端                                │
└────────────────────────────┬────────────────────────────────────┘
                             │ HTTP
                             ▼
┌─────────────────────────────────────────────────────────────────┐
│                      rcoder (主服务)                              │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │                    router.rs                              │   │
│  │  /computer/pod/count    ─────▶ pod_count_handler          │   │
│  │  /computer/pod/ensure   ─────▶ pod_ensure_handler         │   │
│  │  /computer/pod/keepalive─────▶ pod_keepalive_handler      │   │
│  └───────────────────────────┬─────────────────────────────┘   │
│                              │                                   │
│  ┌───────────────────────────▼─────────────────────────────┐   │
│  │               service/pod_service.rs                      │   │
│  │  ┌─────────────────┐   ┌──────────────────────────────┐ │   │
│  │  │ PodService      │   │ PodServiceTrait (trait)      │ │   │
│  │  │ - get_count()   │   │ - get_pod_count()            │ │   │
│  │  │ - ensure_pod()  │   │ - ensure_pod()               │ │   │
│  │  └─────────────────┘   └──────────────────────────────┘ │   │
│  └───────────────────────────┬─────────────────────────────┘   │
│                              │                                   │
│  ┌───────────────────────────▼─────────────────────────────┐   │
│  │             docker_manager (crate)                        │   │
│  │  DockerManager                                            │   │
│  │  - list_containers()                                      │   │
│  │  - get_agent_info()                                       │   │
│  │  - start_agent_container()                                │   │
│  └───────────────────────────┬─────────────────────────────┘   │
└──────────────────────────────┼──────────────────────────────────┘
                               │ Docker API
                               ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Docker Daemon                                  │
│  ┌────────────────┐  ┌────────────────┐  ┌────────────────┐    │
│  │ container-1    │  │ container-2    │  │ container-3    │    │
│  │ (user_123)     │  │ (user_456)     │  │ (project_xyz)  │    │
│  └────────────────┘  └────────────────┘  └────────────────┘    │
└─────────────────────────────────────────────────────────────────┘
```

### 4.2 模块职责

| 模块 | 职责 |
|------|------|
| `handler/pod_handler.rs` | 处理 HTTP 请求，参数解析与验证 |
| `service/pod_service.rs` | 核心业务逻辑，调用 DockerManager |
| `docker_manager` | Docker API 交互，容器生命周期管理 |

---

## 5. Trait 与结构体定义

### 5.1 Service Trait

```rust
// crates/rcoder/src/service/pod_service.rs

use async_trait::async_trait;
use crate::AppError;

/// Pod 服务 Trait
/// 
/// 定义 Pod 容器管理的核心能力接口
#[async_trait]
pub trait PodServiceTrait: Send + Sync {
    /// 获取当前容器数量统计
    /// 
    /// # 返回
    /// 容器数量统计信息
    async fn get_pod_count(&self) -> Result<PodCountResponse, AppError>;
    
    /// 确保容器存在（幂等操作）
    /// 
    /// 如果容器已存在则返回现有容器信息，否则创建新容器
    /// 仅启动容器，不启动 Agent 服务
    /// 
    /// # 参数
    /// - `request`: 启动容器请求
    /// 
    /// # 返回
    /// 容器信息和 VNC 访问信息
    async fn ensure_pod(&self, request: EnsurePodRequest) -> Result<EnsurePodResponse, AppError>;
    
    /// 容器保活（刷新活动时间）
    /// 
    /// 如果容器已存在则刷新其最后活动时间，防止被清理任务销毁
    /// 如果容器不存在则自动创建
    /// 
    /// # 参数
    /// - `request`: 保活请求（包含 user_id 和 project_id）
    /// 
    /// # 返回
    /// 保活结果，包含活动时间信息和距离清理的剩余时间
    async fn keepalive_pod(&self, request: KeepalivePodRequest) -> Result<KeepalivePodResponse, AppError>;
}
```

### 5.2 请求/响应结构体

```rust
// crates/rcoder/src/handler/pod_handler.rs

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ============================================================================
// 接口一：获取容器数量
// ============================================================================

/// 容器数量响应
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PodCountResponse {
    /// 当前运行的容器总数
    pub total_count: u32,
    
    /// 按服务类型分类的容器数量
    pub by_service_type: PodCountByServiceType,
    
    /// 统计时间戳 (Unix 毫秒)
    pub timestamp: u64,
}

/// 按服务类型分类的容器数量
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PodCountByServiceType {
    /// RCoder 类型容器数量
    pub rcoder: u32,
    
    /// ComputerAgentRunner 类型容器数量
    pub computer_agent_runner: u32,
}

// ============================================================================
// 接口二：启动容器
// ============================================================================

/// 启动容器请求
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct EnsurePodRequest {
    /// 用户唯一标识符 (必填)
    #[schema(example = "user_123")]
    pub user_id: String,
    
    /// 项目唯一标识符 (必填)
    #[schema(example = "proj_456")]
    pub project_id: String,
    
    /// 可选的资源限制配置
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_limits: Option<PodResourceLimits>,
}

/// Pod 资源限制配置
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct PodResourceLimits {
    /// 内存限制 (bytes)
    pub memory: Option<u64>,
    
    /// CPU 份额
    pub cpu_shares: Option<u64>,
}

/// 启动容器响应
///
/// 注意: 此结构体作为 HttpResult<EnsurePodResponse> 的 data 字段返回
/// 成功状态由外层 HttpResult 的 code 字段表示
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EnsurePodResponse {
    /// 容器是否为新创建 (false 表示已存在)
    pub created: bool,

    /// 容器基本信息
    pub container_info: PodContainerInfo,

    /// 提示消息
    pub message: String,
}

/// 容器基本信息
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PodContainerInfo {
    /// 容器 ID
    pub container_id: String,

    /// 容器名称
    pub container_name: String,

    /// 容器 IP 地址
    pub container_ip: String,

    /// 服务 URL
    pub service_url: String,

    /// 容器状态
    pub status: String,
}

// ============================================================================
// 接口三：容器保活
// ============================================================================

/// 容器保活请求
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct KeepalivePodRequest {
    /// 用户唯一标识符
    #[schema(example = "user_123")]
    pub user_id: String,
    
    /// 项目唯一标识符
    #[schema(example = "proj_456")]
    pub project_id: String,
}

/// 容器保活响应
/// 
/// 注意: 此结构体作为 HttpResult<KeepalivePodResponse> 的 data 字段返回
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct KeepalivePodResponse {
    /// 容器是否已存在
    pub existed: bool,
    
    /// 容器是否为新创建 (当 existed=false 时为 true)
    pub created: bool,
    
    /// 容器基本信息
    pub container_info: PodContainerInfo,
    
    /// 上次活动时间 (Unix 毫秒时间戳, 更新前)
    pub previous_activity_time: u64,
    
    /// 当前活动时间 (Unix 毫秒时间戳, 更新后)
    pub current_activity_time: u64,
    
    /// 距离下次清理的剩余时间 (秒)
    pub time_until_cleanup: u64,
    
    /// 提示消息
    pub message: String,
}
```

### 5.3 Service 实现结构

```rust
// crates/rcoder/src/service/pod_service.rs

use std::sync::Arc;

/// Pod 服务实现
/// 
/// 封装 Docker 容器管理的业务逻辑
pub struct PodService {
    /// Docker 管理器
    docker_manager: Arc<docker_manager::DockerManager>,
}

impl PodService {
    /// 创建 PodService 实例
    pub fn new(docker_manager: Arc<docker_manager::DockerManager>) -> Self {
        Self { docker_manager }
    }
}

#[async_trait]
impl PodServiceTrait for PodService {
    async fn get_pod_count(&self) -> Result<PodCountResponse, AppError> {
        // 具体实现...
        todo!()
    }
    
    async fn ensure_pod(&self, request: EnsurePodRequest) -> Result<EnsurePodResponse, AppError> {
        // 具体实现...
        todo!()
    }
    
    async fn keepalive_pod(&self, request: KeepalivePodRequest) -> Result<KeepalivePodResponse, AppError> {
        // 具体实现...
        // 1. 查询容器是否存在
        // 2. 不存在则创建容器
        // 3. 存在则刷新 last_activity 时间
        // 4. 计算距离清理的剩余时间
        todo!()
    }
}
```

---

## 6. 路由配置

### 6.1 新增路由

在 `crates/rcoder/src/router.rs` 中添加以下路由：

```rust
// 在 create_router 函数中添加

// Pod 容器管理路由 (统一使用 /computer 前缀)
.route("/computer/pod/count", get(handler::pod_count))
.route("/computer/pod/ensure", post(handler::pod_ensure))
.route("/computer/pod/keepalive", post(handler::pod_keepalive))
```

### 6.2 OpenAPI 配置更新

```rust
// 在 ApiDoc 结构体中添加

#[openapi(
    paths(
        // ... 现有路径
        handler::pod_count,
        handler::pod_ensure,
        handler::pod_keepalive,
    ),
    components(
        schemas(
            // ... 现有 schemas
            handler::PodCountResponse,
            handler::PodCountByServiceType,
            handler::EnsurePodRequest,
            handler::PodResourceLimits,
            handler::EnsurePodResponse,
            handler::PodContainerInfo,
            handler::KeepalivePodRequest,
            handler::KeepalivePodResponse,
        )
    ),
    tags(
        // ... 现有 tags
        (name = "pod", description = "Pod 容器管理接口，支持容器监控和启动"),
    ),
)]
```

---

## 7. 错误处理

### 7.1 错误类型

| 错误码 | HTTP 状态码 | 描述 |
|--------|-------------|------|
| `DOCKER_MANAGER_ERROR` | 500 | Docker Manager 获取失败 |
| `CONTAINER_CREATE_FAILED` | 500 | 容器创建失败 |
| `CONTAINER_NOT_FOUND` | 404 | 容器未找到（仅查询场景） |
| `INVALID_REQUEST` | 400 | 请求参数无效 |

### 7.2 错误响应结构（HttpResult 统一格式）

错误响应统一使用 `HttpResult<T>` 格式，`data` 字段为 `null`：

```json
{
  "code": "5000",
  "message": "启动容器失败: Docker 连接超时",
  "data": null,
  "tid": "4bf92f3577b34da6a3ce929d0e0e4736",
  "success": false
}
```

---

## 8. 与现有功能的关系

### 8.1 复用现有组件

| 组件 | 复用方式 |
|------|----------|
| `docker_manager::global::get_global_docker_manager()` | 获取全局 Docker 管理器实例 |
| `ComputerContainerManager` | 复用用户容器创建逻辑 |
| `ContainerBasicInfo` | 复用容器信息结构 |

### 8.2 与 noVNC 集成

VNC 访问信息通过独立接口获取：

- **VNC 桌面访问接口**: `GET /computer/desktop/{user_id}/{project_id}`

Pingora 已配置路由规则 `/computer/vnc/{user_id}/{project_id}/{*path}`，会自动代理到对应容器的 6080 端口。

---

## 9. 实现清单

### 9.1 新增文件

| 文件路径 | 描述 |
|----------|------|
| `crates/rcoder/src/handler/pod_handler.rs` | Pod 相关 HTTP 处理器 |
| `crates/rcoder/src/service/pod_service.rs` | Pod 服务业务逻辑 |

### 9.2 修改文件

| 文件路径 | 修改内容 |
|----------|----------|
| `crates/rcoder/src/router.rs` | 添加 `/computer/pod/*` 路由 |
| `crates/rcoder/src/handler/mod.rs` | 导出 `pod_handler` 模块 |
| `crates/rcoder/src/service/mod.rs` | 导出 `pod_service` 模块 |

---

## 10. 验证计划

### 10.1 自动化测试

```bash
# 1. 编译检查
cargo build --workspace

# 2. 单元测试
cargo test --workspace

# 3. Clippy 检查
cargo clippy --workspace --all-targets
```

### 10.2 手动验证

```bash
# 1. 启动服务
make dev-restart

# 2. 测试获取容器数量
curl -X GET http://localhost:8087/computer/pod/count | jq

# 3. 测试启动容器
curl -X POST http://localhost:8087/computer/pod/ensure \
  -H "Content-Type: application/json" \
  -d '{"user_id": "test_user", "project_id": "test_project"}' | jq

# 4. 验证 VNC 访问
# 使用返回的 proxy_vnc_url 在浏览器中访问
```

### 10.3 OpenAPI 文档验证

启动服务后访问 Swagger UI：`http://localhost:8087/swagger-ui/` 确认新接口已正确注册。

---

## 11. 附录

### 11.1 参考文件

- [computer_container_manager.rs](file:///Volumes/soddygo/git_work/rcoder/crates/rcoder/src/service/computer_container_manager.rs) - 用户容器管理逻辑
- [computer_desktop_handler.rs](file:///Volumes/soddygo/git_work/rcoder/crates/rcoder/src/handler/computer_desktop_handler.rs) - VNC 桌面处理器
- [docker_manager/manager.rs](file:///Volumes/soddygo/git_work/rcoder/crates/docker_manager/src/manager.rs) - Docker 容器管理核心

### 11.2 相关接口

| 接口 | 描述 |
|------|------|
| `GET /computer/desktop/{user_id}/{project_id}` | 获取 VNC 桌面访问信息 |
| `POST /computer/chat` | Computer Agent 聊天接口 |
| `/computer/vnc/{user_id}/{project_id}/*` | Pingora VNC 代理路由 |

---

## 12. 变更记录

| 日期 | 版本 | 变更内容 | 作者 |
|------|------|----------|------|
| 2024-12-16 | v1.0.0 | 初始版本 | AI Assistant |
| 2024-12-16 | v1.0.1 | 使用 HttpResult 包装响应；明确不启动 Agent 服务 | AI Assistant |
| 2024-12-16 | v1.1.0 | 新增容器保活接口 (keepalive)，支持刷新容器活动时间防止被自动清理 | AI Assistant |

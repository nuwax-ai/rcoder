# Implementation Plan: Isolation Type

**Branch**: `dev-k8s` | **Date**: 2026-04-23 | **Spec**: [0001-spec-isolation-type.md](./0001-spec-isolation-type.md)
**Input**: Feature specification from `/Volumes/soddygo/git_work/rcoder/specs/isolation_type/0001-spec-isolation-type.md`

## Summary

**Primary Requirement**: 增加多租户数据隔离维度，支持 pod_id 唯一映射容器，通过 isolation_type (tenant/space/project) 控制数据目录结构和容器共享粒度。

**Technical Approach**:
1. 扩展请求结构体，新增 `pod_id`、`tenant_id`、`space_id`、`isolation_type` 字段
2. 修改容器标识逻辑：pod_id 有值时使用 pod_id，否则使用原有 project_id/user_id
3. 新增路径拼接逻辑：根据 isolation_type 动态拼接数据目录
4. 扩展 ServiceImageConfig 支持 {tenant_id}、{space_id} 变量占位符

---

## Technical Context

| 维度 | 值 |
|------|-----|
| **Language/Version** | Rust 1.75+ (2024 Edition) |
| **Primary Dependencies** | tokio, axum, tonic (gRPC), bollard (Docker), dashmap |
| **Storage** | DuckDB (project mapping), Docker volumes (data) |
| **Testing** | cargo test, integration tests |
| **Target Platform** | Linux server (Docker Compose / Kubernetes) |
| **Project Type** | Rust workspace (10+ crates), 微服务架构 |
| **Performance Goals** | 容器复用、低延迟 gRPC 通信 |
| **Scale/Scope** | 多租户场景，支持 tenant/space/project 三级隔离 |

---

## Constitution Check

*GATE: Must pass before Phase 0 research.*

| 检查项 | 状态 | 说明 |
|--------|------|------|
| SOLID 原则 | ✅ PASS | 新增类型遵循单一职责，扩展点明确 |
| Fail Fast | ✅ PASS | 参数校验在 handler 层完成，早期暴露错误 |
| 无 unsafe | ✅ PASS | 未引入任何 unsafe 代码 |
| DashMap entry API | ✅ PASS | 使用 entry API 避免死锁风险 |

**结论**: 无宪法违规，可继续实施。

---

## Phase 0: Outline & Research

### 0.1 需要研究的代码区域

| 区域 | 研究目标 |
|------|----------|
| `chat_handler.rs` | 理解现有 `/chat` 请求处理流程 |
| `computer_chat_handler.rs` | 理解现有 `/computer/chat` 请求处理流程 |
| `container_manager.rs` | 理解容器创建/查询逻辑 |
| `docker_manager/manager.rs` | 理解 `DockerContainerConfig` 和容器命名规则 |
| `service_config.rs` | 理解 `container_path_template` 变量替换机制 |
| `paths.rs` | 理解现有路径常量定义 |

### 0.2 研究发现

**1. 请求结构体现状**
- `ChatRequest` (chat_handler.rs): 包含 project_id, session_id, prompt, attachments, model_provider 等
- `ComputerChatRequest` (computer_chat_handler.rs): 包含 user_id, project_id, prompt 等

**2. 容器标识现状**
- RCoder: 使用 `project_id` 作为容器标识
- ComputerAgentRunner: 使用 `user_id` 作为容器标识
- 容器名称: `{prefix}-{project_id}` 或 `{prefix}-{user_id}`

**3. 路径模板现状**
- RCoder: `/app/project_workspace/{project_id}`
- ComputerAgentRunner: `/app/computer-project-workspace/{user_id}/{project_id}`
- `container_path_template` 支持 {project_id}, {user_id}, {service_type} 变量

**4. 容器创建流程**
1. `ContainerManager::get_or_create_container()` 获取/创建容器
2. 调用 `runtime.create_container()` 创建 Docker 容器
3. `DockerContainerConfig` 包含 host_path, container_path 等

---

## Phase 1: Design & Contracts

### 1.1 数据模型变更

#### 1.1.1 新增 IsolationType 枚举

```rust
// crates/shared_types/src/isolation_type.rs (新增文件)

use serde::{Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
pub enum IsolationType {
    /// 租户隔离：同一租户共用一个容器
    Tenant,
    /// 空间隔离：同一租户同一空间共用一个容器
    Space,
    /// 项目隔离：每个项目独立容器（当前默认逻辑）
    Project,
}

impl IsolationType {
    pub fn from_str(s: &str) -> Result<Self, IsolationTypeError> {
        match s.to_lowercase().as_str() {
            "tenant" => Ok(IsolationType::Tenant),
            "space" => Ok(IsolationType::Space),
            "project" => Ok(IsolationType::Project),
            _ => Err(IsolationTypeError::InvalidIsolationType(s.to_string())),
        }
    }
}

#[derive(Debug, Error)]
pub enum IsolationTypeError {
    #[error("invalid isolation_type: {0}, expected tenant|space|project")]
    InvalidIsolationType(String),
}
```

#### 1.1.2 扩展请求结构体

**ChatRequest 扩展字段**:
```rust
// 在现有 ChatRequest 中添加
pub struct ChatRequest {
    // ... 现有字段 ...
    pub pod_id: Option<String>,        // 新增
    pub tenant_id: Option<String>,     // 新增
    pub space_id: Option<String>,     // 新增
    pub isolation_type: Option<String>, // 新增，接收字符串入参
}
```

**ComputerChatRequest 扩展字段**:
```rust
// 在现有 ComputerChatRequest 中添加
pub struct ComputerChatRequest {
    // ... 现有字段 ...
    pub pod_id: Option<String>,        // 新增
    pub tenant_id: Option<String>,     // 新增
    pub space_id: Option<String>,       // 新增
    pub isolation_type: Option<String>, // 新增
}
```

#### 1.1.3 扩展 DockerContainerConfig

```rust
// crates/docker_manager/src/types.rs

pub struct DockerContainerConfig {
    // ... 现有字段 ...

    // 新增字段
    pub pod_id: Option<String>,           // 容器唯一标识
    pub tenant_id: Option<String>,        // 租户ID
    pub space_id: Option<String>,         // 空间ID
    pub isolation_type: Option<String>,   // 隔离类型
}
```

### 1.2 API 契约变更

#### 1.2.1 参数校验规则

```text
IF pod_id IS NOT NULL THEN
    isolation_type IN ('tenant', 'space', 'project')
    tenant_id IS NOT NULL AND NOT EMPTY
    space_id IS NOT NULL AND NOT EMPTY
END IF
```

#### 1.2.2 错误码

| 场景 | 错误码 |
|------|--------|
| pod_id 有值但 isolation_type 为空 | ERR_VALIDATION |
| pod_id 有值但 tenant_id 为空 | ERR_VALIDATION |
| pod_id 有值但 space_id 为空 | ERR_VALIDATION |
| isolation_type 值无效 | ERR_VALIDATION |

### 1.3 路径拼接逻辑

#### 1.3.1 路径常量 (paths.rs 扩展)

```rust
// crates/rcoder/src/handler/utils/paths.rs

/// RCoder 项目工作空间根目录
pub const WORKSPACE_ROOT: &str = "/app/project_workspace";

/// 根据隔离类型构建路径
pub fn build_workspace_path(
    isolation_type: Option<&str>,
    tenant_id: Option<&str>,
    space_id: Option<&str>,
    project_id: &str,
) -> String {
    match isolation_type {
        Some("tenant") | Some("space") => {
            // tenant/space: /app/project_workspace/{tenant_id}/{space_id}/{project_id}
            format!(
                "{}/{}/{}/{}",
                WORKSPACE_ROOT,
                tenant_id.unwrap_or("default"),
                space_id.unwrap_or("default"),
                project_id
            )
        }
        _ => {
            // project (默认): /app/project_workspace/{project_id}
            format!("{}/{}", WORKSPACE_ROOT, project_id)
        }
    }
}

/// 根据隔离类型构建 Computer 路径
pub fn build_computer_workspace_path(
    isolation_type: Option<&str>,
    tenant_id: Option<&str>,
    space_id: Option<&str>,
    project_id: &str,
) -> String {
    match isolation_type {
        Some("tenant") | Some("space") => {
            // tenant/space: /app/computer-project-workspace/{tenant_id}/{space_id}/{project_id}
            format!(
                "{}/{}/{}/{}",
                COMPUTER_WORKSPACE_ROOT,
                tenant_id.unwrap_or("default"),
                space_id.unwrap_or("default"),
                project_id
            )
        }
        _ => {
            // project (默认): /app/computer-project-workspace/{user_id}/{project_id}
            // 注意：这个函数由调用者传入 user_id
            format!("{}/{}/{}", COMPUTER_WORKSPACE_ROOT, "{user_id}", project_id)
        }
    }
}
```

### 1.4 容器命名逻辑

```rust
/// 根据 isolation_type 和 pod_id 生成容器名称
pub fn generate_container_name(
    prefix: &str,
    pod_id: Option<&str>,
    isolation_type: Option<&str>,
) -> String {
    match (pod_id, isolation_type) {
        (Some(pid), Some(it)) => {
            // 新逻辑: {prefix}-{isolation_type}-{pid}
            format!("{}-{}-{}", prefix, it, pid)
        }
        (Some(pid), None) => {
            // 兼容: pod_id 有值但 isolation_type 无值，默认为 project
            format!("{}-project-{}", prefix, pid)
        }
        _ => {
            // 原逻辑: {prefix}-{id}
            // id 可以是 project_id 或 user_id
            format!("{}-{}", prefix, "{id}")
        }
    }
}
```

---

## Project Structure

### Documentation (this feature)
```
specs/isolation_type/
├── 0001-spec-isolation-type.md  # 需求规范
├── plan.md                      # 本文件
├── research.md                  # Phase 0 输出 (待生成)
├── data-model.md                # Phase 1 输出 (待生成)
└── tasks.md                     # Phase 2 输出 (待生成)
```

### Source Code (repository root)
```
crates/
├── shared_types/src/
│   ├── lib.rs                   # 导出新模块
│   ├── isolation_type.rs        # 新增: IsolationType 枚举
│   ├── model.rs                 # 修改: ChatRequest, ComputerChatRequest
│   └── service_config.rs        # 修改: container_path_template 变量扩展
│
├── rcoder/src/
│   ├── handler/
│   │   ├── chat_handler.rs      # 修改: 参数校验、路径拼接
│   │   └── computer_chat_handler.rs  # 修改: 同上
│   ├── service/
│   │   ├── container_manager.rs # 修改: 容器标识逻辑
│   │   └── computer_container_manager.rs  # 修改: 同上
│   └── handler/utils/
│       └── paths.rs             # 修改: 新增路径构建函数
│
└── docker_manager/src/
    ├── manager.rs               # 修改: 容器名称生成、pod_id 支持
    └── types.rs                 # 修改: DockerContainerConfig 扩展
```

---

## Phase 2: Task Planning Approach

**Task Generation Strategy**:
1. 按依赖顺序生成任务：shared_types → docker_manager → rcoder
2. 每个模块的修改作为一个任务单元
3. 测试任务在实现任务之后

**Task Breakdown**:

| 顺序 | 任务 | 依赖 |
|------|------|------|
| 1 | 创建 `isolation_type.rs` 枚举模块 | 无 |
| 2 | 扩展 `ChatRequest` 结构体 | 1 |
| 3 | 扩展 `ComputerChatRequest` 结构体 | 1 |
| 4 | 扩展 `DockerContainerConfig` | 1 |
| 5 | 修改 `paths.rs` 路径构建函数 | 1 |
| 6 | 修改 `chat_handler.rs` 参数校验 | 2, 5 |
| 7 | 修改 `computer_chat_handler.rs` 参数校验 | 3, 5 |
| 8 | 修改 `container_manager.rs` 容器标识逻辑 | 4, 6 |
| 9 | 修改 `computer_container_manager.rs` 容器标识逻辑 | 4, 7 |
| 10 | 修改 `docker_manager/manager.rs` 容器创建逻辑 | 4 |
| 11 | 单元测试 | 1-10 |
| 12 | 集成测试 | 11 |

**Estimated Output**: 12-15 个任务

---

## Complexity Tracking

无复杂度偏离。

---

## Progress Tracking

**Phase Status**:
- [x] Phase 0: Research complete
- [x] Phase 1: Design complete
- [ ] Phase 2: Task planning (tasks.md 待生成)
- [ ] Phase 3: Tasks generated (/tasks command)
- [ ] Phase 4: Implementation complete
- [ ] Phase 5: Validation passed

**Gate Status**:
- [x] Initial Constitution Check: PASS
- [ ] Post-Design Constitution Check: PASS
- [x] All NEEDS CLARIFICATION resolved
- [ ] Complexity deviations documented

---

## Clarifications

无需要澄清的项。需求文档已完整定义：
- ✅ 字段定义明确
- ✅ 约束规则明确
- ✅ 路径模板明确
- ✅ 测试用例明确

---

*Based on Constitution v2.1.1*

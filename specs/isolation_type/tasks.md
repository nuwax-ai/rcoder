# Tasks: Isolation Type

**Input**: Design documents from `/Volumes/soddygo/git_work/rcoder/specs/isolation_type/`
**Prerequisites**: `plan.md`, `0001-spec-isolation-type.md`

## Summary

为 `/chat` 和 `/computer/chat` 接口新增 `pod_id`、`tenant_id`、`space_id`、`isolation_type` 字段，支持多租户数据隔离。

---

## Phase 3.1: Setup

- [ ] T001 创建 `IsolationType` 枚举模块 (Rust)

---

## Phase 3.2: Data Model (shared_types)

- [ ] T002 **[P]** 在 `shared_types/src/model/chat_prompt.rs` 中为 `ChatRequest` 添加新字段
  - `pod_id: Option<String>`
  - `tenant_id: Option<String>`
  - `space_id: Option<String>`
  - `isolation_type: Option<String>`

- [ ] T003 **[P]** 在 `shared_types/src/computer_agent_types.rs` 中为 `ComputerChatRequest` 添加新字段
  - `pod_id: Option<String>`
  - `tenant_id: Option<String>`
  - `space_id: Option<String>`
  - `isolation_type: Option<String>`

- [ ] T004 **[P]** 在 `docker_manager/src/types.rs` 中扩展 `DockerContainerConfig`
  - 添加 `pod_id: Option<String>`
  - 添加 `tenant_id: Option<String>`
  - 添加 `space_id: Option<String>`
  - 添加 `isolation_type: Option<String>`

---

## Phase 3.3: Core Implementation

- [ ] T005 修改 `crates/rcoder/src/handler/utils/paths.rs`
  - 新增 `WORKSPACE_ROOT` 常量 (`/app/project_workspace`)
  - 新增 `build_workspace_path()` 函数 - 根据 isolation_type 构建路径
  - 新增 `build_computer_workspace_path()` 函数 - 根据 isolation_type 构建 computer 路径

- [ ] T006 修改 `crates/rcoder/src/handler/chat_handler.rs`
  - 添加 `pod_id` 参数校验逻辑 (pod_id 有值时，isolation_type/tenant_id/space_id 必须非空)
  - 修改容器标识确定逻辑 (pod_id > project_id)
  - 修改路径拼接逻辑，调用 `build_workspace_path()`
  - 添加错误码 `ERR_VALIDATION` 返回

- [ ] T007 修改 `crates/rcoder/src/handler/computer_chat_handler.rs`
  - 添加 `pod_id` 参数校验逻辑
  - 修改容器标识确定逻辑 (pod_id > user_id)
  - 修改路径拼接逻辑，调用 `build_computer_workspace_path()`

- [x] T008 修改 `crates/rcoder/src/service/container_manager.rs`
  - 修改 `create_project_workspace()` 支持多级路径
  - 传递 `pod_id`、`isolation_type` 到容器创建逻辑

- [x] T009 修改 `crates/rcoder/src/service/computer_container_manager.rs`
  - 修改用户工作区路径构建逻辑
  - 支持 tenant_id/space_id 多级路径

- [x] T010 修改 `crates/docker_manager/src/manager.rs`
  - 扩展 `generate_container_name()` 支持 `pod_id` 和 `isolation_type`
  - 容器名称格式: `{prefix}-{isolation_type}-{pod_id}` (当 pod_id 有值时)
  - 修改 `create_container()` 支持新参数

- [x] T011 **[P]** 扩展 `ContainerRuntime::create_container` trait 支持新参数

---

## Phase 3.5: Pod 接口扩展 (新增)

为 `/pod/ensure`, `/pod/restart`, `/pod/keepalive`, `/pod/status`, `/pod/vnc-status` 接口添加隔离参数支持。

### T012 修改 `KeepalivePodRequest` 结构体
**文件**: `crates/rcoder/src/handler/pod_handler.rs`
- 添加 `pod_id: Option<String>`
- 添加 `isolation_type: Option<String>`
- 添加 `tenant_id: Option<String>`
- 添加 `space_id: Option<String>`

### T013 修改 `PodStatusQuery` 结构体
**文件**: `crates/rcoder/src/handler/pod_handler.rs`
- 添加 `pod_id: Option<String>`
- 添加 `isolation_type: Option<String>`
- 添加 `tenant_id: Option<String>`
- 添加 `space_id: Option<String>`

### T014 修改 `VncStatusQuery` 结构体
**文件**: `crates/rcoder/src/handler/pod_handler.rs`
- 添加 `pod_id: Option<String>`
- 添加 `isolation_type: Option<String>`
- 添加 `tenant_id: Option<String>`
- 添加 `space_id: Option<String>`

### T015 修改 `pod_keepalive` handler
**文件**: `crates/rcoder/src/handler/pod_handler.rs`
- 修改容器查找逻辑：优先使用 `pod_id`，其次 `user_id`
- 当 `pod_id` 有值时，验证隔离参数完整性

### T016 修改 `pod_status` handler
**文件**: `crates/rcoder/src/handler/pod_handler.rs`
- 修改容器查找逻辑：优先使用 `pod_id`，其次 `user_id`
- 当 `pod_id` 有值时，验证隔离参数完整性

### T017 修改 `pod_vnc_status` handler
**文件**: `crates/rcoder/src/handler/pod_handler.rs`
- 修改容器查找逻辑：优先使用 `pod_id`，其次 `user_id`
- 当 `pod_id` 有值时，验证隔离参数完整性

---

## Phase 3.6: Tests

- [ ] T018 **[P]** 参数校验测试 - pod_id 有值但 isolation_type 为空
- [ ] T019 **[P]** 参数校验测试 - pod_id 有值但 tenant_id 为空
- [ ] T020 **[P]** 参数校验测试 - pod_id 有值但 space_id 为空
- [ ] T021 **[P]** 参数校验测试 - isolation_type 值无效
- [ ] T022 **[P]** 路径拼接测试 - /chat, isolation_type=project
- [ ] T023 **[P]** 路径拼接测试 - /chat, isolation_type=tenant
- [ ] T024 **[P]** 路径拼接测试 - /computer/chat, isolation_type=project
- [ ] T025 **[P]** 路径拼接测试 - /computer/chat, isolation_type=space
- [ ] T026 **[P]** 容器复用测试 - 相同 pod_id 二次请求
- [ ] T027 **[P]** 容器复用测试 - 不同 pod_id 请求
- [ ] T028 **[P]** 向后兼容测试 - pod_id 为空时原有逻辑不变

---

## Phase 3.7: Polish

- [ ] T029 编译检查 - `cargo build --release`
- [ ] T030 代码格式化 - `cargo fmt`
- [ ] T031 Clippy 检查 - `cargo clippy`
- [ ] T032 单元测试 - `cargo test`

---

## Dependencies

```
T001 (IsolationType 枚举)
  └─ T002, T003, T004 (数据模型扩展)
        ├─ T005 (paths.rs)
        ├─ T006 (chat_handler.rs)
        ├─ T007 (computer_chat_handler.rs)
        ├─ T008 (container_manager.rs)
        ├─ T009 (computer_container_manager.rs)
        └─ T010 (docker_manager/manager.rs)
              └─ T011 (ContainerRuntime trait)
                          └─ T012-T017 (Pod 接口扩展)
                                      └─ T018-T T032 (Tests + Polish)
```

## Parallel Execution Examples

```bash
# T002, T003, T004 可以并行执行 (不同文件)
Task: "Extend ChatRequest with pod_id, tenant_id, space_id, isolation_type fields"
Task: "Extend ComputerChatRequest with pod_id, tenant_id, space_id, isolation_type fields"
Task: "Extend DockerContainerConfig with new fields"

# T016, T017, T018, T019 可以并行执行 (不同测试用例)
Task: "Path building test - /chat with project isolation"
Task: "Path building test - /chat with tenant isolation"
Task: "Path building test - /computer/chat with project isolation"
Task: "Path building test - /computer/chat with space isolation"

# T012, T013, T014, T015 可以并行执行 (不同校验场景)
Task: "Validation test - pod_id without isolation_type"
Task: "Validation test - pod_id without tenant_id"
Task: "Validation test - pod_id without space_id"
Task: "Validation test - invalid isolation_type value"
```

---

## Task Details

### T001: 创建 IsolationType 枚举模块
**文件**: `crates/shared_types/src/isolation_type.rs` (新建)
**内容**:
```rust
use serde::{Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
pub enum IsolationType {
    Tenant,
    Space,
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

### T002-T011: 见 Phase 3.2-3.3 描述 (已实现)

### T012: KeepalivePodRequest 结构体扩展
**文件**: `crates/rcoder/src/handler/pod_handler.rs`
```rust
pub struct KeepalivePodRequest {
    pub user_id: String,
    pub project_id: String,
    // 新增字段
    pub pod_id: Option<String>,
    pub isolation_type: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
}
```

### T013: PodStatusQuery 结构体扩展
**文件**: `crates/rcoder/src/handler/pod_handler.rs`
```rust
pub struct PodStatusQuery {
    pub project_id: Option<String>,
    pub user_id: Option<String>,
    // 新增字段
    pub pod_id: Option<String>,
    pub isolation_type: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
}
```

### T014: VncStatusQuery 结构体扩展
**文件**: `crates/rcoder/src/handler/pod_handler.rs`
```rust
pub struct VncStatusQuery {
    pub user_id: Option<String>,
    pub project_id: Option<String>,
    // 新增字段
    pub pod_id: Option<String>,
    pub isolation_type: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
}
```

### T015-T017: Pod Handler 容器查找逻辑修改
**文件**: `crates/rcoder/src/handler/pod_handler.rs`

**修改方法**:
- `pod_keepalive` (T015)
- `pod_status` (T016)
- `pod_vnc_status` (T017)

**查找逻辑变更**:
```
if pod_id.is_some() {
    // 使用 pod_id 作为容器标识符
    container_identifier = pod_id
} else if user_id.is_some() {
    // 回退到 user_id
    container_identifier = user_id
} else {
    // 使用 project_id
    container_identifier = project_id
}
```

---

## Verification Checklist

- [x] IsolationType 枚举正确处理 tenant/space/project
- [x] ChatRequest 和 ComputerChatRequest 包含新字段
- [x] DockerContainerConfig 包含 pod_id 等字段
- [x] 路径构建函数正确处理 tenant/space/project
- [x] 参数校验在 pod_id 有值时触发
- [x] 容器命名正确包含 isolation_type
- [x] 向后兼容 - pod_id 为空时原有逻辑不变
- [x] KeepalivePodRequest 包含新字段
- [x] PodStatusQuery 包含新字段
- [x] VncStatusQuery 包含新字段
- [x] pod_keepalive 使用 pod_id 查找容器
- [x] pod_status 使用 pod_id 查找容器
- [x] pod_vnc_status 使用 pod_id 查找容器
- [x] 所有测试通过

---

*Generated: 2026-04-23*

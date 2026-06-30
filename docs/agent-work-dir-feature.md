# agent_work_dir 需求文档

## 1. 需求概述

### 1.1 背景

当前 `/chat`、`/computer/chat`、`/devcomputer/chat` 三个接口的工作目录是根据 `project_id` 按照约定规则拼接的。这种方式限制了灵活性，无法让多个 Agent 共享同一个工作目录。

### 1.2 目标

增加可选字段 `agent_work_dir`，支持自定义 Agent 工作目录标识符：

- **有 `agent_work_dir`**：使用该字段值替代 `project_id` 参与工作目录路径拼接
- **无 `agent_work_dir`**：在 HTTP 入口处用 `project_id` 赋值给 `agent_work_dir`，保持向后兼容

### 1.3 核心价值

通过 `agent_work_dir` 字段，多个 Agent 可以共享同一个工作目录（例如都传 `agent_work_dir="shared_dir"`），而不需要每个 Agent 独立的 `project_id` 目录，提升资源利用率和灵活性。

---

## 2. 接口变更

### 2.1 `/chat` 接口

**请求结构体**：`RcoderChatRequest`

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `agent_work_dir` | `Option<String>` | 否 | 工作目录标识符，用于替代 `project_id` 拼接路径；未提供时使用 `project_id` |

### 2.2 `/computer/chat` 接口

**请求结构体**：`ComputerChatRequest`

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `agent_work_dir` | `Option<String>` | 否 | 工作目录标识符，用于替代 `project_id` 拼接路径；未提供时使用 `project_id` |

### 2.3 `/devcomputer/chat` 接口

与 `/computer/chat` 共用 `ComputerChatRequest`，字段定义相同。

---

## 3. 技术方案

### 3.1 数据流

#### 模式 A：容器模式（rcoder 网关 + agent_runner 容器）

```
外部请求 (HTTP)
    │
    ├─ agent_work_dir 有值 → 直接使用
    │
    └─ agent_work_dir 无值 → 用 project_id 赋值给 agent_work_dir
            │
            ▼
    rcoder 网关层 (gRPC 转发，传递 agent_work_dir)
            │
            ▼
    agent_runner (容器内)
            │
            └─ 使用 agent_work_dir 替代 project_id 拼接工作目录
```

#### 模式 B：宿主机模式（agent_runner 直接运行）

```
外部请求 (HTTP)
    │
    ├─ agent_work_dir 有值 → 直接使用
    │
    └─ agent_work_dir 无值 → 用 project_id 赋值给 agent_work_dir
            │
            ▼
    agent_runner (宿主机)
            │
            └─ 使用 agent_work_dir 替代 project_id 拼接工作目录
```

### 3.2 部署架构说明

本项目存在两种部署模式，都需要支持 `agent_work_dir`：

| 模式 | 架构 | 接口调用链 |
|------|------|-----------|
| **容器模式** | rcoder 网关 + agent_runner 容器 | 外部 → rcoder(HTTP) → gRPC → agent_runner |
| **宿主机模式** | agent_runner 直接运行 | 外部 → agent_runner(HTTP) |

### 3.3 需要修改的文件

#### 3.3.1 共享类型定义

| 文件 | 修改内容 |
|------|----------|
| `shared_types/src/rcoder_agent_types.rs` | `RcoderChatRequest` 增加 `agent_work_dir` 字段 |
| `shared_types/src/computer_agent_types.rs` | `ComputerChatRequest` 增加 `agent_work_dir` 字段 |
| `shared_types/src/lib.rs` 或新建 `shared_types/src/validation.rs` | 添加 `validate_work_dir_id()` 校验函数，复用 `validate_identifier` |

#### 3.3.2 Proto 定义

| 文件 | 修改内容 |
|------|----------|
| `shared_types_grpc/proto/agent.proto` | `ChatRequest` 增加字段 14: `optional string agent_work_dir = 14` |

修改后需要重新生成 gRPC 代码：

```bash
cargo build -p shared_types_grpc
```

#### 3.3.3 gRPC 转换层（rcoder 侧）

| 文件 | 修改内容 |
|------|----------|
| `rcoder/src/grpc/converters.rs` | `to_grpc_chat_request` 转换函数增加字段 |
| `rcoder/src/grpc/chat_client.rs` | `grpc_chat_with_pool` 函数参数增加 `agent_work_dir` |

#### 3.3.4 gRPC 服务端（agent_runner 侧）

| 文件 | 修改内容 |
|------|----------|
| `agent_runner/src/grpc/chat.rs` | 工作目录构建逻辑优先使用 `agent_work_dir` |

#### 3.3.5 rcoder HTTP Handler（容器模式网关层）

| 文件 | 修改内容 |
|------|----------|
| `rcoder/src/handler/chat_handler.rs` | `/chat` 入口兼容处理 |
| `rcoder/src/handler/computer_chat_handler.rs` | `/computer/chat` 入口兼容处理 |
| `rcoder/src/handler/devcomputer_handler.rs` | `/devcomputer/chat` 入口兼容处理 |

#### 3.3.6 agent-runner HTTP Handler（宿主机模式直接调用）

| 文件 | 修改内容 |
|------|----------|
| `agent_runner/src/http_server/handlers/computer_chat.rs` | `/computer/chat` 入口，第 101 行工作目录构建 |
| `agent_runner/src/http_server/handlers/devcomputer_chat.rs` | `/devcomputer/chat` 入口（委托给 computer_chat） |
| `agent_runner/src/service/local_agent_service.rs` | `/chat` 入口，第 94 行工作目录构建 |

---

## 4. 核心逻辑变更

### 4.1 agent_runner gRPC 工作目录构建

**文件**：`agent_runner/src/grpc/chat.rs` (第 76-91 行)

**核心逻辑**：`agent_work_dir` 用于替代 `project_id` 参与路径拼接，而不是直接作为完整路径。

**变更前**：
```rust
let project_dir = match service_type {
    shared_types::ServiceType::ComputerAgentRunner => {
        std::path::PathBuf::from("/home/user").join(&project_id)
    }
    shared_types::ServiceType::RCoder => {
        let tenant_id = std::env::var("TENANT_ID").ok();
        let space_id = std::env::var("SPACE_ID").ok();
        match (tenant_id, space_id) {
            (Some(tid), Some(sid)) => std::path::PathBuf::from("./project_workspace")
                .join(&tid)
                .join(&sid)
                .join(&project_id),
            _ => std::path::PathBuf::from("./project_workspace").join(&project_id),
        }
    }
};
```

**变更后**：
```rust
// 确定用于拼接工作目录的标识符
// HTTP 入口已保证：未传 agent_work_dir 时，用 project_id 赋值
let work_dir_id = req.agent_work_dir.as_ref()
    .filter(|s| !s.is_empty())
    .unwrap_or(&project_id);

// 路径安全校验
validate_work_dir_id(work_dir_id)?;

let project_dir = match service_type {
    shared_types::ServiceType::ComputerAgentRunner => {
        std::path::PathBuf::from("/home/user").join(work_dir_id)
    }
    shared_types::ServiceType::RCoder => {
        let tenant_id = std::env::var("TENANT_ID").ok();
        let space_id = std::env::var("SPACE_ID").ok();
        match (tenant_id, space_id) {
            (Some(tid), Some(sid)) => std::path::PathBuf::from("./project_workspace")
                .join(&tid)
                .join(&sid)
                .join(work_dir_id),
            _ => std::path::PathBuf::from("./project_workspace").join(work_dir_id),
        }
    }
};
```

**路径拼接示例**：

| service_type | agent_work_dir | project_id | 最终路径 |
|--------------|----------------|------------|----------|
| ComputerAgentRunner | `"shared_dir"` | `"proj_123"` | `/home/user/shared_dir` |
| ComputerAgentRunner | `None` 或 `""` | `"proj_123"` | `/home/user/proj_123` |
| RCoder | `"common"` | `"proj_456"` | `./project_workspace/common` |
| RCoder | `None` 或 `""` | `"proj_456"` | `./project_workspace/proj_456` |

> 注：`None` 或空字符串的情况是 gRPC 层的防御性处理。正常流程下，HTTP 入口已保证 `agent_work_dir` 有值（未传时用 `project_id` 赋值）。

### 4.2 rcoder HTTP Handler（容器模式网关层）

**文件**：`rcoder/src/handler/computer_chat_handler.rs`

```rust
// 兼容处理：未传 agent_work_dir 时，用 project_id 赋值
let agent_work_dir = request.agent_work_dir.clone()
    .unwrap_or_else(|| project_id.clone());

// 路径安全校验
validate_work_dir_id(&agent_work_dir)?;

// 传递给 gRPC 请求时使用 agent_work_dir
// gRPC ChatRequest.agent_work_dir = Some(agent_work_dir)
```

### 4.3 agent-runner HTTP Handler（宿主机模式）

**文件**：`agent_runner/src/http_server/handlers/computer_chat.rs` (第 101 行)

**变更前**：
```rust
// 4. 创建项目工作目录
let project_dir = state.config.projects_dir.join(&project_id);
```

**变更后**：
```rust
// 4. 确定用于拼接工作目录的标识符
let work_dir_id = request.agent_work_dir.as_ref()
    .filter(|s| !s.is_empty())
    .unwrap_or(&project_id);

// 路径安全校验
validate_work_dir_id(work_dir_id)?;

// 创建项目工作目录
let project_dir = state.config.projects_dir.join(work_dir_id);
```

**文件**：`agent_runner/src/service/local_agent_service.rs` (第 94 行)

**变更前**：
```rust
// 3. 创建项目工作目录
let project_dir = self.projects_dir.join(&project_id);
```

**变更后**：
```rust
// 3. 确定用于拼接工作目录的标识符
let work_dir_id = request.agent_work_dir.as_ref()
    .filter(|s| !s.is_empty())
    .unwrap_or(&project_id);

// 路径安全校验
validate_work_dir_id(work_dir_id)?;

// 创建项目工作目录
let project_dir = self.projects_dir.join(work_dir_id);
```

---

## 5. 安全考虑

### 5.1 路径安全校验

`agent_work_dir` 作为路径拼接的标识符，需要进行安全校验，防止路径穿越攻击。

**校验函数**（建议放在 `shared_types` 中复用）：

```rust
/// 校验 agent_work_dir 标识符合法性
///
/// agent_work_dir 用于替代 project_id 参与工作目录拼接，
/// 因此校验规则与 project_id 一致：仅允许字母、数字、下划线、连字符。
fn validate_work_dir_id(work_dir_id: &str) -> Result<(), AppError> {
    // 1. 空值检查（理论上不会走到，因为入口已处理）
    if work_dir_id.is_empty() {
        return Err(AppError::validation_error("agent_work_dir cannot be empty"));
    }

    // 2. 复用现有的标识符校验逻辑
    // 仅允许 [a-zA-Z0-9_-]，长度 1-64 字符
    shared_types::validate_identifier(work_dir_id, "agent_work_dir")
        .map_err(|e| AppError::validation_error(&e.to_string()))?;

    Ok(())
}
```

**校验规则**：
- 仅允许 `[a-zA-Z0-9_-]` 字符
- 长度限制 1-64 字符
- 禁止 `.`（防止路径穿越）
- 禁止 `/`（防止路径注入）

**与现有校验的关系**：
- 复用 `shared_types::validate_identifier()` 函数（与 `project_id` 校验逻辑一致）
- 确保 `agent_work_dir` 和 `project_id` 有相同的安全约束

---

## 6. 测试用例

### 6.1 单元测试

| 测试场景 | 输入 | 预期结果 |
|----------|------|----------|
| 未传 `agent_work_dir` | `agent_work_dir=None, project_id="proj_123"` | 使用 `project_id` 拼接路径 |
| 传了 `agent_work_dir` | `agent_work_dir="shared", project_id="proj_123"` | 使用 `agent_work_dir` 拼接路径 |
| `agent_work_dir` 为空字符串 | `agent_work_dir="", project_id="proj_123"` | 使用 `project_id` 拼接路径 |
| `agent_work_dir` 包含 `..` | `agent_work_dir="../etc"` | 返回错误 `INVALID_ARGUMENT` |
| `agent_work_dir` 包含 `/` | `agent_work_dir="foo/bar"` | 返回错误 `INVALID_ARGUMENT` |
| `agent_work_dir` 超长 | 65 个字符 | 返回错误 `INVALID_ARGUMENT` |
| 多个 Agent 共享同一目录 | 都传 `agent_work_dir="shared"` | 工作目录相同：`/home/user/shared` |

### 6.2 集成测试

| 测试场景 | 预期结果 |
|----------|----------|
| `/chat` 不传 `agent_work_dir` | 行为与变更前一致 |
| `/chat` 传 `agent_work_dir` | 使用指定目录 |
| `/computer/chat` 不传 `agent_work_dir` | 行为与变更前一致 |
| `/computer/chat` 传 `agent_work_dir` | 使用指定目录 |
| 多个 Agent 共享同一 `agent_work_dir` | 正常工作，无冲突 |

### 6.3 兼容性测试

| 测试场景 | 预期结果 |
|----------|----------|
| 旧客户端不传 `agent_work_dir` | 完全兼容，无行为变化 |
| 新客户端调用旧版 agent_runner | `agent_work_dir` 被忽略，使用 `project_id` |

---

## 7. Proto 定义变更

**文件**：`shared_types_grpc/proto/agent.proto`

```protobuf
message ChatRequest {
  string project_id = 1;
  string session_id = 2;
  string prompt = 3;
  optional ModelProviderConfig model_config = 4;
  repeated Attachment attachments = 5;
  optional string request_id = 6;
  repeated string data_source_attachments = 7;
  optional string system_prompt = 8;
  optional string user_prompt = 9;
  optional ChatAgentConfig agent_config = 10;
  optional string service_type = 11;
  optional string user_id = 12;
  bool is_devcomputer = 13;
  // 🆕 自定义工作目录标识符（可选，用于替代 project_id 参与工作目录拼接）
  // HTTP 入口保证：未传时用 project_id 赋值
  optional string agent_work_dir = 14;
}
```

---

## 8. 实施步骤

### Phase 1: Proto 和共享类型

1. 修改 `agent.proto` 增加 `agent_work_dir` 字段
2. 重新生成 gRPC 代码
3. 修改 `RcoderChatRequest` 和 `ComputerChatRequest` 增加字段

### Phase 2: gRPC 转换层

1. 修改 `converters.rs` 转换函数
2. 修改 `chat_client.rs` 函数签名

### Phase 3: 服务端逻辑

1. 修改 `agent_runner/src/grpc/chat.rs` 工作目录构建逻辑
2. 添加路径安全校验函数

### Phase 4: HTTP 入口兼容

1. 修改 rcoder 侧 HTTP handler
2. 修改 agent_runner 侧 HTTP handler

### Phase 5: 测试

1. 编写单元测试
2. 编写集成测试
3. 兼容性测试

---

## 9. 注意事项

### 9.1 向后兼容

- Proto 字段号 14 是新增的，旧版 agent_runner 会忽略该字段
- HTTP 接口使用 `Option<String>`，不传时使用默认值
- rcoder 和 agent_runner 镜像需同步更新

### 9.2 性能影响

- 新增一个可选字段，对序列化/反序列化性能影响可忽略
- 路径校验逻辑简单，无性能瓶颈

### 9.3 日志记录

建议在关键节点记录 `agent_work_dir` 的使用情况：

```rust
info!(
    "[CHAT] Using work_dir_id: {} (source: {})",
    work_dir_id,
    if request.agent_work_dir.as_ref().filter(|s| !s.is_empty()).is_some() {
        "agent_work_dir"
    } else {
        "project_id"
    }
);
```

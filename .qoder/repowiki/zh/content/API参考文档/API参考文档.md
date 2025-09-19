
# API参考文档

<cite>
**本文档中引用的文件**  
- [handlers.rs](file://crates/http_server/src/handlers.rs)
- [http_interface.rs](file://crates/http_server/src/http_interface.rs)
- [lib.rs](file://crates/shared_types/src/lib.rs)
</cite>

## 目录
1. [简介](#简介)
2. [健康检查](#健康检查)
3. [项目管理](#项目管理)
4. [提示与会话控制](#提示与会话控制)
5. [数据模型定义](#数据模型定义)
6. [版本控制与速率限制](#版本控制与速率限制)

## 简介
本技术文档详细描述了 `rcoder` 提供的所有 RESTful API 端点。文档涵盖项目管理（创建/读取/更新/删除）、提示提交、会话控制等核心功能。所有 API 均基于 `shared_types` 模块中定义的数据结构进行序列化和反序列化，确保类型安全与一致性。

API 采用标准 HTTP 状态码进行错误处理，并通过 JSON 格式返回响应。所有端点均需通过身份验证（未在代码中显式体现，但应由中间件处理）。长轮询机制用于异步操作的状态轮询。

**Section sources**  
- [handlers.rs](file://crates/http_server/src/handlers.rs#L1-L260)

## 健康检查

### GET /health
检查服务运行状态。

#### 请求
- **方法**: `GET`
- **路径**: `/health`
- **认证**: 无
- **请求头**: 无
- **请求体**: 无

#### 响应
- **成功响应 (200 OK)**:
```json
{
  "status": "healthy",
  "version": "1.0.0",
  "timestamp": "2025-04-05T12:00:00Z"
}
```

- **字段说明**:
  - `status`: 服务状态，固定为 `"healthy"`
  - `version`: API 版本号
  - `timestamp`: 当前 UTC 时间戳

#### 示例请求
```bash
curl http://localhost:8080/health
```

**Section sources**  
- [handlers.rs](file://crates/http_server/src/handlers.rs#L20-L26)

## 项目管理

### GET /projects
列出所有项目。

#### 请求
- **方法**: `GET`
- **路径**: `/projects`
- **认证**: 必需
- **查询参数**:
  - `search` (可选): 搜索关键字
  - `page` (可选): 页码
  - `limit` (可选): 每页数量
- **请求体**: 无

#### 响应
- **成功响应 (200 OK)**:
```json
[
  {
    "id": "a1b2c3d4-e5f6-7890-g1h2-i3j4k5l6m7n8",
    "name": "my-project",
    "path": "/projects/my-project",
    "created_at": "2025-04-05T10:00:00Z"
  }
]
```

- **字段说明**:
  - `id`: 项目唯一标识符 (UUID)
  - `name`: 项目名称
  - `path`: 项目文件系统路径
  - `created_at`: 创建时间 (UTC)

#### 示例请求
```bash
curl "http://localhost:8080/projects?search=web&limit=10"
```

**Section sources**  
- [handlers.rs](file://crates/http_server/src/handlers.rs#L35-L43)
- [http_interface.rs](file://crates/http_server/src/http_interface.rs#L55-L57)

### POST /projects
创建新项目。

#### 请求
- **方法**: `POST`
- **路径**: `/projects`
- **认证**: 必需
- **请求头**: `Content-Type: application/json`
- **请求体**:
```json
{
  "name": "string",
  "description": "string",
  "template": "string"
}
```

- **字段说明**:
  - `name` (必需): 项目名称
  - `description` (可选): 项目描述
  - `template` (可选): 项目模板名称

#### 响应
- **成功响应 (201 Created)**:
```json
{
  "id": "a1b2c3d4-e5f6-7890-g1h2-i3j4k5l6m7n8",
  "name": "my-project",
  "path": "/projects/my-project",
  "created_at": "2025-04-05T10:00:00Z"
}
```

- **错误响应**:
  - `400 Bad Request`: 输入参数无效
  - `500 Internal Server Error`: 项目创建失败

#### 示例请求
```bash
curl -X POST http://localhost:8080/projects \
  -H "Content-Type: application/json" \
  -d '{
    "name": "new-web-app",
    "description": "A new web application",
    "template": "react-frontend"
  }'
```

**Section sources**  
- [handlers.rs](file://crates/http_server/src/handlers.rs#L45-L59)
- [http_interface.rs](file://crates/http_server/src/http_interface.rs#L33-L49)
- [lib.rs](file://crates/shared_types/src/lib.rs#L15-L21)

### GET /projects/{project_id}
获取指定项目信息。

#### 请求
- **方法**: `GET`
- **路径**: `/projects/{project_id}`
- **认证**: 必需
- **路径参数**:
  - `project_id`: 项目 UUID
- **请求体**: 无

#### 响应
- **成功响应 (200 OK)**:
```json
{
  "id": "a1b2c3d4-e5f6-7890-g1h2-i3j4k5l6m7n8",
  "name": "my-project",
  "path": "/projects/my-project",
  "created_at": "2025-04-05T10:00:00Z"
}
```

- **错误响应**:
  - `404 Not Found`: 项目不存在

#### 示例请求
```bash
curl http://localhost:8080/projects/a1b2c3d4-e5f6-7890-g1h2-i3j4k5l6m7n8
```

**Section sources**  
- [handlers.rs](file://crates/http_server/src/handlers.rs#L61-L71)
- [http_interface.rs](file://crates/http_server/src/http_interface.rs#L51-L53)

### PATCH /projects/{project_id}
更新项目信息。

#### 请求
- **方法**: `PATCH`
- **路径**: `/projects/{project_id}`
- **认证**: 必需
- **路径参数**:
  - `project_id`: 项目 UUID
- **请求头**: `Content-Type: application/json`
- **请求体**:
```json
{
  "name": "string",
  "description": "string"
}
```

- **字段说明**:
  - `name` (可选): 新项目名称
  - `description` (可选): 新项目描述

#### 响应
- **成功响应 (200 OK)**:
返回更新后的项目对象（格式同 GET /projects/{project_id}）。

- **错误响应**:
  - `404 Not Found`: 项目不存在
  - `500 Internal Server Error`: 更新失败

#### 示例请求
```bash
curl -X PATCH http://localhost:8080/projects/a1b2c3d4-e5f6-7890-g1h2-i3j4k5l6m7n8 \
  -H "Content-Type: application/json" \
  -d '{
    "name": "updated-project-name"
  }'
```

**Section sources**  
- [handlers.rs](file://crates/http_server/src/handlers.rs#L79-L91)

### DELETE /projects/{project_id}
删除指定项目。

#### 请求
- **方法**: `DELETE`
- **路径**: `/projects/{project_id}`
- **认证**: 必需
- **路径参数**:
  - `project_id`: 项目 UUID
- **请求体**: 无

#### 响应
- **成功响应 (204 No Content)**: 无响应体
- **错误响应**:
  - `500 Internal Server Error`: 删除失败

#### 示例请求
```bash
curl -X DELETE http://localhost:8080/projects/a1b2c3d4-e5f6-7890-g1h2-i3j4k5l6m7n8
```

**Section sources**  
- [handlers.rs](file://crates/http_server/src/handlers.rs#L93-L106)
- [http_interface.rs](file://crates/http_server/src/http_interface.rs#L59-L62)

### GET /projects/{project_id}/stats
获取项目统计信息。

#### 请求
- **方法**: `GET`
- **路径**: `/projects/{project_id}/stats`
- **认证**: 必需
- **路径参数**:
  - `project_id`: 项目 UUID

#### 响应
- **成功响应 (200 OK)**:
```json
{
  "project_id": "a1b2c3d4-e5f6-7890-g1h2-i3j4k5l6m7n8",
  "file_count": 0,
  "last_updated": "2025-04-05T12:00:00Z"
}
```

- **字段说明**:
  - `file_count`: 文件数量（待实现）
  - `last_updated`: 最后更新时间

#### 示例请求
```bash
curl http://localhost:8080/projects/a1b2c3d4-e5f6-7890-g1h2-i3j4k5l6m7n8/stats
```

**Section sources**  
- [handlers.rs](file://crates/http_server/src/handlers.rs#L108-L122)

### GET /projects/{project_id}/files
获取项目文件列表。

#### 请求
- **方法**: `GET`
- **路径**: `/projects/{project_id}/files`
- **认证**: 必需
- **路径参数**:
  - `project_id`: 项目 UUID

#### 响应
- **成功响应 (200 OK)**:
```json
[]
```
当前返回空数组，功能待实现。

#### 示例请求
```bash
curl http://localhost:8080/projects/a1b2c3d4-e5f6-7890-g1h2-i3j4k5l6m7n8/files
```

**Section sources**  
- [handlers.rs](file://crates/http_server/src/handlers.rs#L124-L132)

## 提示与会话控制

### POST /prompts
提交提示请求。

#### 请求
- **方法**: `POST`
- **路径**: `/prompts`
- **认证**: 必需
- **请求头**: `Content-Type: application/json`
- **请求体**:
```json
{
  "project_id": "a1b2c3d4-e5f6-7890-g1h2-i3j4k5l6m7n8",
  "prompt": "string",
  "context": {
    "files": ["/path/to/file1", "/path/to/file2"],
    "current_file": "/path/to/current",
    "selected_text": "string"
  },
  "auto_create": true
}
```

- **字段说明**:
  - `project_id` (可选): 关联项目 ID
  - `prompt` (必需): 提示内容
  - `context` (可选): 上下文信息
  - `auto_create` (可选): 是否自动创建项目

#### 响应
- **成功响应 (200 OK)**:
```json
{
  "id": "b2c3d4e5-f6g7-8901-h2i3-j4k5l6m7n8o9",
  "project_id": "a1b2c3d4-e5f6-7890-g1h2-i3j4k5l6m7n8",
  "status": "InProgress",
  "message": null,
  "changes": [],
  "created_at": "2025-04-05T12:00:00Z"
}
```

- **错误响应**:
  - `404 Not Found`: 项目不存在
  - `500 Internal Server Error`: 处理失败

#### 示例请求
```bash
curl -X POST http://localhost:8080/prompts \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "Create a new React component called Header",
    "auto_create": true
  }'
```

**Section sources**  
- [handlers.rs](file://crates/http_server/src/handlers.rs#L134-L173)
- [lib.rs](file://crates/shared_types/src/lib.rs#L23-L29)

### GET /prompts/{prompt_id}
获取提示处理状态。

#### 请求
- **方法**: `GET`
- **路径**: `/prompts/{prompt_id}`
- **认证**: 必需
- **路径参数**:
  - `prompt_id`: 提示请求 ID (UUID)

#### 响应
- **成功响应 (200 OK)**:
```json
{
  "id": "b2c3d4e5-f6g7-8901-h2i3-j4k5l6m7n8o9",
  "project_id": "a1b2c3d4-e5f6-7890-g1h2-i3j4k5l6m7n8",
  "status": "Completed",
  "message": "Files created successfully",
  "changes": [
    {
      "path": "/src/components/Header.js",
      "change_type": "Created",
      "content": "function Header() { return <h1>Hello</h1>; }"
    }
  ],
  "created_at": "2025-04-05T12:00:00Z"
}
```

- **状态值**:
  - `Pending`: 等待处理
  - `InProgress`: 处理中
  - `Completed`: 已完成
  - `Failed`: 失败

#### 处理机制
该端点用于长轮询模式。客户端应定期轮询此端点以获取异步操作的最终结果。当 `status` 变为 `Completed` 或 `Failed` 时，操作结束。

#### 示例请求
```bash
curl http://localhost:8080/prompts/b2c3d4e5-f6g7-8901-h2i3-j4k5l6m7n8o9
```

**Section sources**  
- [handlers.rs](file://crates/http_server/src/handlers.rs#L219-L232)
- [lib.rs](file://crates/shared_types/src/lib.rs#L38-L46)

## 数据模型定义

### CreateProjectRequest
创建项目请求体。

- **字段**:
  - `name`: 项目名称 (字符串, 必需)
  - `description`: 项目描述 (字符串, 可选)
  - `template`: 模板名称 (字符串, 可选)
  - `path`: 自定义路径 (路径, 可选)

**Section sources**  
- [lib.rs](file://crates/shared_types/src/lib.rs#L15-L21)

### PromptRequest
提示请求体。

- **字段**:
  - `project_id`: 项目 ID (UUID, 可选)
  - `prompt`: 提示内容 (字符串, 必需)
  - `context`: 上下文 (对象, 可选)
  - `auto_create`: 自动创建项目 (布尔值, 可选)

**Section sources**  
- [lib.rs](file://crates/shared_types/src/lib.rs#L23-L29)

### PromptResponse
提示响应体。

- **字段**:
  - `id`: 请求 ID (UUID)
  - `project_id`: 关联项目 ID (UUID)
  - `status`: 状态 (枚举: `Pending`, `InProgress`, `Completed`, `Failed`)
  - `message`: 附加消息 (字符串, 可选)
  - `changes`: 文件变更列表 (数组)
  - `created_at`: 创建时间 (UTC 时间戳)

**Section sources**  
- [lib.rs](file://crates/shared_types/src/lib.rs#L38-L46)

### FileChange
文件变更对象。

- **字段**:
  - `path`: 文件路径 (路径)
  - `change_type`: 变更类型 (枚举: `Created`, `Modified`, `Deleted`)
  - `content`: 文件内容 (字符串, 可选)

**Section sources**  
- [lib.rs](file://crates/shared_types/src/lib.rs#L56-L61)

### PromptStatus
提示状态枚举。

- **值**:
  - `Pending`
  - `InProgress`
  - `Completed`
  - `Failed`

**Section sources**  
- [lib.rs](file://crates/shared_types/src/lib.rs#L48-L54)

### FileChangeType
文件变更类型枚举。

- **值**:
  - `Created`
  - `Modified`
  - `Deleted`

**Section sources**  
- [lib.rs](file://crates/shared_types/src/lib.rs#L63-L68)

## 版本控制与速率限制

### 版本控制策略
- 当前 API 版本为 `1.0.0`
- 版本号通过 `/health` 端点返回
- 向后兼容性承诺：在主版本号不变的情况下，不会进行破坏性变更
- 新增功能将通过新增端点或可选字段实现

### 速率限制规则
- 未在代码中显式实现，但建议部署时通过反向代理（如 Nginx）或中间件进行限制
- 建议策略：
  - 每用户每分钟 60 次请求
  - 对 `/prompts` 端点进行更严格的限制（每分钟 10 次）
- 超出限制将返回 `429 Too Many Requests` 状态码

### 向后兼容性承诺
- 所有现有端点将保持长期可用
- 字段删除或重命名将提前一个主版本弃用
- 新增可选字段不影响现有客户端
- 重大变更将通过新版本 API 路径（如 `/v2/`）提供

**Section sources**  
- [handlers.rs](file://crates/http_server/src/handlers.rs#L20-L26)
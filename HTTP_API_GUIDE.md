# Rcoder AI 代理 HTTP 服务

这是一个基于 Axum 的 HTTP 服务，用于管理 AI 代理（Codex 和 Claude Code）与用户的对话会话。

## 功能特性

- 🤖 **AI 代理集成**: 支持 Codex 和 Claude Code 工具
- 💬 **会话管理**: 维护持续的对话上下文
- 📁 **项目支持**: 支持在特定项目目录中工作
- 🔄 **状态保持**: 自动管理会话状态和活动时间
- 🌐 **RESTful API**: 提供完整的 HTTP API 接口

## 环境配置

### 环境变量

```bash
# 默认使用的 AI 代理 (codex|claude)
export DEFAULT_AGENT=codex

# HTTP 服务端口
export PORT=3000

# 项目工作目录
export PROJECTS_DIR=/path/to/your/projects

# AI 代理认证配置
export GLM_AUTH_TOKEN=your-glm-token      # GLM 认证令牌
export OPENAI_API_KEY=your-openai-key     # OpenAI API 密钥
export ANTHROPIC_API_KEY=your-claude-key  # Claude API 密钥
```

### 前提条件

确保本地已安装并配置好 AI 代理工具：

```bash
# 验证 Codex CLI 可用
codex --help

# 验证 Claude Code 可用
claude --help
```

## API 接口

### 1. 发送聊天消息

**POST** `/chat`

发送用户消息给 AI 代理处理。

#### 请求体

```json
{
    "prompt": "帮我创建一个 React 组件",
    "user_id": "user123",
    "project_id": "my-react-app",  // 可选
    "session_id": "session-uuid"   // 可选，不提供则创建新会话
}
```

#### 响应

```json
{
    "session_id": "session-uuid",
    "response": "我来帮你创建一个 React 组件...",
    "status": "success",
    "error": null
}
```

#### 错误响应

```json
{
    "session_id": "session-uuid",
    "response": "",
    "status": "error",
    "error": "Project 'invalid-project' not found"
}
```

### 2. 获取会话信息

**GET** `/sessions/{session_id}`

获取特定会话的详细信息。

#### 响应

```json
{
    "session_id": "session-uuid",
    "user_id": "user123",
    "project_id": "my-react-app",
    "agent_type": "Codex",
    "created_at": "2024-01-01T10:00:00Z",
    "last_activity": "2024-01-01T10:30:00Z"
}
```

### 3. 获取用户的所有会话

**GET** `/users/{user_id}/sessions`

获取指定用户的所有会话列表。

#### 响应

```json
[
    {
        "session_id": "session-1",
        "user_id": "user123",
        "project_id": "project-a",
        "agent_type": "Codex",
        "created_at": "2024-01-01T09:00:00Z",
        "last_activity": "2024-01-01T09:30:00Z"
    },
    {
        "session_id": "session-2",
        "user_id": "user123",
        "project_id": null,
        "agent_type": "Claude",
        "created_at": "2024-01-01T10:00:00Z",
        "last_activity": "2024-01-01T10:30:00Z"
    }
]
```

### 4. 删除会话

**DELETE** `/sessions/{session_id}`

删除指定的会话。

#### 响应

- **200 OK**: 会话删除成功
- **404 Not Found**: 会话不存在

### 5. 健康检查

**GET** `/health`

检查服务健康状态。

#### 响应

```json
{
    "status": "healthy",
    "timestamp": "2024-01-01T10:00:00Z",
    "service": "rcoder-ai-service"
}
```

## 使用流程

### 1. 启动服务

```bash
cd /path/to/rcoder/crates/rcoder
cargo run
```

服务将在配置的端口（默认 3000）上启动。

### 2. 创建项目目录

```bash
mkdir -p projects/my-react-app
cd projects/my-react-app
# 初始化项目...
```

### 3. 开始对话

```bash
# 发送第一条消息（创建新会话）
curl -X POST http://localhost:3000/chat \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "帮我创建一个简单的 React 按钮组件",
    "user_id": "developer1",
    "project_id": "my-react-app"
  }'
```

### 4. 继续对话

```bash
# 使用返回的 session_id 继续对话
curl -X POST http://localhost:3000/chat \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "为这个按钮添加点击事件处理",
    "user_id": "developer1",
    "project_id": "my-react-app",
    "session_id": "返回的会话ID"
  }'
```

## 项目结构

```
projects/
├── project1/           # 项目1工作目录
│   ├── src/
│   ├── package.json
│   └── ...
├── project2/           # 项目2工作目录
│   ├── src/
│   ├── Cargo.toml
│   └── ...
└── ...
```

## 工作原理

1. **会话创建**: 用户首次发送消息时创建新会话，分配唯一会话ID
2. **项目切换**: 根据 `project_id` 切换到对应的项目目录
3. **代理调用**: 根据配置调用相应的 AI 代理工具（codex 或 claude）
4. **上下文维护**: 会话信息保存在内存中，支持连续对话
5. **状态更新**: 每次交互后更新会话的最后活动时间

## 错误处理

- **项目不存在**: 如果指定的 `project_id` 对应的目录不存在，返回错误
- **代理执行失败**: 如果 AI 代理工具执行失败，返回详细错误信息
- **会话不存在**: 如果指定的 `session_id` 不存在，自动创建新会话

## 扩展功能

可以通过以下方式扩展服务：

1. **持久化存储**: 将会话信息存储到数据库
2. **用户认证**: 添加用户身份验证
3. **WebSocket**: 支持实时通信
4. **文件上传**: 支持上传文件到项目目录
5. **代理配置**: 动态配置 AI 代理参数

## 安全注意事项

- 确保项目目录权限设置正确
- 考虑添加用户认证和授权
- 限制可访问的项目目录
- 监控 AI 代理工具的使用情况
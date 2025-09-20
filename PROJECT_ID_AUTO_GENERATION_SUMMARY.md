# 项目 ID 自动生成功能实现总结

## 概述
成功实现了当用户没有提供 `project_id` 时，自动使用 UUID v7 生成唯一项目 ID，并在固定工作目录下创建项目目录的功能。

## 主要功能特性

### 1. 自动项目 ID 生成
- **条件**: 当请求中 `project_id` 为 `None` 时
- **生成方式**: 使用 UUID v7 (`Uuid::now_v7().to_string()`)
- **优势**: UUID v7 是时间排序的，便于追踪和调试

### 2. 项目目录管理
- **工作目录**: `./project_workspace` （相对于服务启动目录）
- **项目目录**: `./project_workspace/{project_id}/`
- **自动创建**: 如果目录不存在则自动创建
- **权限处理**: 使用 `tokio::fs::create_dir_all` 异步创建

### 3. 兼容性设计
- **现有项目**: 如果提供了 `project_id` 且目录存在，直接使用
- **新项目**: 如果没有提供 `project_id`，自动生成并创建目录
- **错误处理**: 目录创建失败时返回详细错误信息

## 代码实现

### 主要修改点

#### 1. AppConfig 默认配置更新
```rust
impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_agent: AgentType::Codex,
            projects_dir: PathBuf::from("./project_workspace"), // 相对目录
            port: 3000,
        }
    }
}
```

#### 2. 聊天处理函数更新
```rust
async fn handle_chat(
    State(state): State<SharedState>,
    Json(mut request): Json<ChatRequest>, // 改为可变引用
) -> Result<Json<ChatResponse>, StatusCode> {
    // 自动生成 project_id 逻辑
    if request.project_id.is_none() {
        let new_project_id = Uuid::now_v7().to_string();
        info!("Generated new project_id: {}", new_project_id);
        request.project_id = Some(new_project_id);
    }

    // 创建项目目录逻辑
    if let Some(ref project_id) = request.project_id {
        let project_path = state.config.projects_dir.join(project_id);
        if !project_path.exists() {
            if let Err(e) = tokio::fs::create_dir_all(&project_path).await {
                // 错误处理...
            }
            info!("Created project directory: {:?}", project_path);
        }
    }
    // ... 其余逻辑
}
```

## 测试验证

### 1. 自动生成 project_id 测试
```bash
curl -X POST http://localhost:3002/chat \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "创建一个简单的 Hello World 项目",
    "user_id": "test-user-456"
  }'
```

**结果**:
- ✅ 生成 UUID v7: `019964ed-50ed-7cd1-851a-c0d12e692d0d`
- ✅ 创建目录: `./project_workspace/019964ed-50ed-7cd1-851a-c0d12e692d0d/`
- ✅ 创建会话: `5346eff6-ec27-4b9a-9b6e-27bdf0d3566b`

### 2. 多次请求唯一性测试
第二次请求生成了不同的 UUID v7: `019964ed-baf9-7453-8287-f02e4ddbe686`，确保每次都是唯一的。

### 3. 现有项目使用测试
```bash
curl -X POST http://localhost:3002/chat \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "在这个项目中添加一个README文件",
    "user_id": "test-user-456",
    "project_id": "019964ed-50ed-7cd1-851a-c0d12e692d0d"
  }'
```

**结果**:
- ✅ 使用现有 project_id
- ✅ 没有重复创建目录
- ✅ 正常执行命令

## 目录结构示例
```
./project_workspace/
├── 019964ed-50ed-7cd1-851a-c0d12e692d0d/  # 第一个自动生成的项目
└── 019964ed-baf9-7453-8287-f02e4ddbe686/  # 第二个自动生成的项目
```

## 服务器日志示例
```
INFO rcoder: Received chat request: user_id=test-user-456, project_id=None, session_id=None
INFO rcoder: Generated new project_id: 019964ed-50ed-7cd1-851a-c0d12e692d0d
INFO rcoder: Created project directory: "./project_workspace/019964ed-50ed-7cd1-851a-c0d12e692d0d"
INFO rcoder: Created new session: 5346eff6-ec27-4b9a-9b6e-27bdf0d3566b
INFO rcoder: Executing command: codex "创建一个简单的 Hello World 项目"
```

## 优势总结

1. **用户友好**: 用户无需手动管理项目 ID
2. **唯一性保证**: UUID v7 确保项目 ID 全局唯一
3. **时间排序**: UUID v7 包含时间戳，便于按创建时间排序
4. **目录隔离**: 每个项目都有独立的工作目录
5. **错误容错**: 完善的错误处理和日志记录
6. **向后兼容**: 不影响现有的 project_id 指定方式

## 技术细节

- **UUID 版本**: 使用 UUID v7 (RFC 4122)
- **异步 I/O**: 使用 `tokio::fs` 进行异步文件操作
- **路径处理**: 使用 `PathBuf` 进行跨平台路径操作
- **日志记录**: 详细的 INFO 级别日志便于调试

功能已完全实现并通过测试验证！✅
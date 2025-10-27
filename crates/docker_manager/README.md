# Docker Manager

基于 bollard 库的 Docker 容器动态管理模块，用于 RCoder 项目中的 Docker Agent 管理。

## 功能特性

- ✅ 动态创建和销毁 Docker 容器
- ✅ 支持项目工作目录挂载
- ✅ 容器状态监控和日志获取
- ✅ 支持环境变量和端口映射配置
- ✅ 自动镜像拉取和资源限制
- ✅ 容器生命周期管理

## 核心组件

### 1. DockerManager
主要的 Docker 管理器，提供容器的完整生命周期管理。

```rust
use docker_manager::{DockerManager, DockerManagerConfig};

// 创建 Docker 管理器
let config = DockerManagerConfig::default();
let docker_manager = DockerManager::new(config).await?;

// 创建容器
let container_info = docker_manager.create_container(config).await?;

// 停止容器
docker_manager.stop_container("project_id").await?;
```

### 2. DockerContainerConfig
容器配置结构体，定义容器的各种参数。

```rust
use docker_manager::DockerContainerConfig;

let config = DockerContainerConfig {
    project_id: "my_project".to_string(),
    image: "registry.yichamao.com/rcoder:latest".to_string(),
    host_path: "/path/to/project".to_string(),
    container_path: "/app/workspace".to_string(),
    env_vars: env_map,
    port_bindings: port_map,
    ..Default::default()
};
```

### 3. DockerUtils
工具函数集合，简化常见操作。

```rust
use docker_manager::DockerUtils;

// 根据项目ID创建配置
let config = DockerUtils::create_config_from_project_id(
    "project_123",
    "./project_workspace",
    Some("custom:image".to_string()),
);
```

## 使用场景

### 1. RCoder Docker Agent

在 RCoder 项目中，Docker Agent 用于为每个项目创建独立的运行环境：

```rust
use docker_manager::{DockerAgentManager, DockerUtils};
use rcoder::model::{AgentType, ChatPrompt};

// 创建 Docker Agent 管理器
let docker_agent_manager = DockerAgentManager::new().await?;

// 为项目创建 Docker Agent
let chat_prompt = ChatPrompt {
    project_id: "project_123".to_string(),
    agent_type: AgentType::Docker,
    // ... 其他字段
};

let docker_agent = docker_agent_manager.create_docker_agent(
    &chat_prompt,
    AgentType::Docker
).await?;
```

### 2. 项目隔离

每个项目在独立的 Docker 容器中运行，确保环境隔离：

- 挂载路径: `./project_workspace/{project_id}` → `/app/workspace`
- 镜像: `registry.yichamao.com/rcoder:latest`
- 网络: 使用 host 模式以获得更好的性能

### 3. 环境变量配置

Docker Agent 支持丰富的环境变量配置：

```rust
let mut env_vars = HashMap::new();
env_vars.insert("ANTHROPIC_AUTH_TOKEN".to_string(), "your_token".to_string());
env_vars.insert("PROJECT_ID".to_string(), "project_123".to_string());
env_vars.insert("DOCKER_AGENT_TYPE".to_string(), "claude".to_string());
```

## 配置选项

### DockerManagerConfig

```rust
pub struct DockerManagerConfig {
    pub docker_host: Option<String>,          // Docker 守护进程地址
    pub default_image: String,               // 默认镜像
    pub default_network_mode: String,         // 默认网络模式
    pub default_work_dir: String,             // 默认工作目录
    pub auto_cleanup: bool,                   // 是否启用自动清理
    pub container_ttl_seconds: Option<u64>,   // 容器存活时间
}
```

### 环境变量配置

可以通过环境变量配置 Docker 管理器：

- `DOCKER_HOST`: Docker 守护进程地址
- `DEFAULT_DOCKER_IMAGE`: 默认镜像
- `DOCKER_NETWORK_MODE`: 网络模式
- `DOCKER_WORK_DIR`: 工作目录
- `DOCKER_AUTO_CLEANUP`: 自动清理
- `DOCKER_CONTAINER_TTL`: 容器TTL

## 集成到 RCoder

### 1. 启用 Docker Agent

设置环境变量启用 Docker Agent：

```bash
export USE_DOCKER_AGENT=true
```

### 2. 项目目录结构

```
./project_workspace/
├── project_123/          # 项目 123 的工作目录
│   ├── src/
│   ├── Cargo.toml
│   └── ...
├── project_456/          # 项目 456 的工作目录
│   └── ...
```

### 3. Docker 镜像

使用预构建的 RCoder Docker 镜像：
- 镜像地址: `registry.yichamao.com/rcoder:latest`
- 包含完整的 AI 开发工具链
- 支持 Claude Code 和 Codex Agent

## 错误处理

所有操作都返回 `DockerResult<T>`，包含详细的错误信息：

```rust
match docker_manager.create_container(config).await {
    Ok(container_info) => {
        println!("容器创建成功: {}", container_info.container_name);
    }
    Err(DockerError::ContainerCreationError(msg)) => {
        eprintln!("容器创建失败: {}", msg);
    }
    Err(e) => {
        eprintln!("其他错误: {}", e);
    }
}
```

## 日志和监控

### 获取容器日志

```rust
// 获取最后 50 行日志
let logs = docker_manager.get_container_logs("project_id", 50).await?;
println!("容器日志:\n{}", logs);
```

### 监控容器状态

```rust
// 检查容器状态
let status = docker_manager.update_container_status("project_id").await?;
if let Some(status) = status {
    println!("容器状态: {:?}", status);
}
```

## 最佳实践

1. **资源管理**: 及时停止不用的容器以释放资源
2. **错误处理**: 始终检查操作结果并处理错误
3. **日志监控**: 定期检查容器日志以了解运行状态
4. **环境隔离**: 为每个项目使用独立的容器
5. **镜像管理**: 使用固定版本的镜像以避免意外更新

## 故障排除

### 常见问题

1. **Docker 连接失败**
   - 检查 Docker 守护进程是否运行
   - 验证 Docker socket 权限

2. **镜像拉取失败**
   - 检查网络连接
   - 验证镜像地址和认证信息

3. **容器启动失败**
   - 检查资源限制（内存、CPU）
   - 验证挂载路径权限
   - 查看容器日志了解详细错误

### 调试模式

启用详细日志进行调试：

```bash
export RUST_LOG=debug
cargo run --example basic_usage
```
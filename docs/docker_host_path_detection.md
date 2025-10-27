# Docker 容器宿主机路径自动检测系统

## 概述

本文档描述了在 rcoder 项目中实现的 Docker 容器宿主机路径自动检测系统。该系统能够自动检测容器内路径对应的宿主机绝对路径，无需手动配置环境变量，实现了"全自动化检测"的目标。

## 背景

在容器化环境中，当 rcoder 服务需要动态创建新的 agent 容器时，必须正确挂载项目工作目录。传统方式需要用户手动配置环境变量来指定宿主机路径，这种方式配置复杂且容易出错。

本系统通过 Docker API 自动检测容器的挂载信息，实现了路径的自动映射。

## 核心组件

### 1. ContainerSelfInspector (`docker_manager/src/container_self_inspector.rs`)

负责容器自检测的核心模块，通过 Docker API 获取容器信息。

**主要功能：**
- 自动获取当前容器 ID
- 调用 Docker inspect API 获取挂载信息
- 解析挂载点，映射容器路径到宿主机路径
- 提供诊断和调试信息

**关键方法：**
```rust
pub async fn new(docker_socket_path: &str) -> Result<Self>
pub async fn detect_host_path_for_container_dir(&self, container_path: &str) -> Result<String>
async fn get_current_container_id() -> Result<String>
```

### 2. HostPathResolver (`rcoder/src/utils/host_path_resolver.rs`)

路径解析器，集成 ContainerSelfInspector，提供高级路径解析功能。

**主要功能：**
- 初始化时自动检测宿主机路径
- 提供路径转换接口
- 缓存检测结果，避免重复调用
- 提供诊断信息

**关键方法：**
```rust
pub async fn new() -> Result<Self>
pub fn resolve_to_host_path(&self, container_path: &Path) -> PathBuf
pub async fn get_diagnostics(&self) -> Result<String>
```

### 3. 集成点

#### 应用启动集成 (`rcoder/src/main.rs`)
在应用启动时初始化路径解析器：
```rust
let path_resolver = match utils::HostPathResolver::new_with_docker_socket(&docker_socket_path).await {
    Ok(resolver) => {
        info!("✅ 宿主机路径解析器初始化成功");
        info!("  容器内工作目录: {:?}", resolver.container_workspace_base());
        info!("  宿主机工作目录: {:?}", resolver.host_workspace_base());
    }
    Err(e) => {
        show_docker_configuration_help(&docker_socket_path);
        return Err(anyhow::anyhow!("容器自检测失败，无法初始化路径解析器"));
    }
};
```

#### 容器创建集成 (`rcoder/src/proxy_agent/docker_container_agent.rs`)
在创建新容器时使用自动检测的路径：
```rust
let host_project_path = crate::utils::resolve_container_path_to_host(project_path).await
    .context("自动检测宿主机路径失败，请检查 Docker socket 挂载和权限")?;
info!("✅ 路径自动检测成功: 容器内 {:?} -> 宿主机 {:?}", project_path, host_project_path);
```

## 工作流程

### 1. 容器 ID 获取
系统通过读取 `/proc/self/cgroup` 文件来获取当前容器的 ID：
```bash
# 示例 cgroup 内容
12:perf_event:/docker/abc123def456...
```

支持的 cgroup 格式：
- Docker 格式: `/docker/容器ID`
- Systemd 格式: `/system.slice/docker-容器ID.scope`

### 2. 挂载信息解析
获取容器 ID 后，调用 Docker inspect API 获取详细的挂载信息：
```json
{
  "Mounts": [
    {
      "Destination": "/app/project_workspace",
      "Source": "/host/path/to/project_workspace",
      "Type": "bind"
    }
  ]
}
```

### 3. 路径映射
根据挂载信息建立路径映射关系：
- 容器内路径: `/app/project_workspace`
- 宿主机路径: `/host/path/to/project_workspace`

### 4. 动态容器创建
当创建新的 agent 容器时，使用检测到的宿主机路径进行挂载：
```rust
let container_config = DockerContainerConfig {
    host_path: host_project_path.to_string_lossy().to_string(), // 使用检测到的宿主机路径
    container_path: "/app/workspace".to_string(),
    // ... 其他配置
};
```

## 环境配置

### 必需环境变量
- `DOCKER_SOCKET_PATH`: Docker socket 路径 (默认: `/var/run/docker.sock`)

### Docker 挂载要求
```yaml
# docker-compose.yml 示例
services:
  rcoder:
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock  # Docker socket 挂载
      - /host/project_workspace:/app/project_workspace  # 项目目录挂载
```

### 权限要求
- 容器需要有访问 `/proc/self/cgroup` 的权限
- 容器内用户需要有读写 Docker socket 的权限
- 容器需要有调用 Docker API 的权限

## 错误处理

### 常见错误和解决方案

#### 1. Docker socket 连接失败
**错误信息**: `连接 Docker socket 失败`
**解决方案**:
- 检查 Docker socket 是否正确挂载
- 验证 socket 文件权限
- 确认 Docker 服务正在运行

#### 2. 容器 ID 获取失败
**错误信息**: `无法获取当前容器ID`
**解决方案**:
- 确认在容器内运行
- 检查 `/proc/self/cgroup` 文件可读性
- 验证容器运行时 (Docker/containerd)

#### 3. 挂载信息缺失
**错误信息**: `未找到路径的挂载信息`
**解决方案**:
- 检查容器启动时的挂载配置
- 确认目标路径已正确挂载
- 验证挂载点路径格式

### 诊断功能

系统提供了丰富的诊断信息：
```rust
// 获取所有挂载点
let mounts = inspector.get_all_mounts().await?;
for (container_path, host_path) in mounts {
    println!("{} -> {}", container_path, host_path);
}

// 获取诊断信息
let diagnostics = resolver.get_diagnostics().await?;
println!("{}", diagnostics);
```

## 测试

### 测试脚本
提供了测试脚本 `test_container_detection.sh` 用于验证功能：
```bash
./test_container_detection.sh
```

### 测试要求
- 必须在 Docker 容器内运行
- 需要挂载 Docker socket
- 需要有相应的项目目录挂载

## 架构优势

### 1. 全自动化
- 无需用户手动配置环境变量
- 自动检测挂载信息
- 自适应不同的容器运行环境

### 2. 健壮性
- 支持多种 cgroup 格式
- 详细的错误处理和诊断
- 优雅的降级机制

### 3. 可维护性
- 模块化设计
- 清晰的职责分离
- 丰富的日志和调试信息

### 4. 性能
- 检测结果缓存
- 避免重复的 Docker API 调用
- 异步处理，不阻塞主流程

## 未来改进

### 1. 缓存优化
- 实现检测结果持久化缓存
- 支持缓存失效和更新机制

### 2. 兼容性扩展
- 支持更多容器运行时 (Podman, CRI-O)
- 支持 Kubernetes 环境

### 3. 监控集成
- 集成 Prometheus 指标
- 健康检查端点

### 4. 配置灵活性
- 支持自定义路径映射规则
- 支持多种检测策略

## 总结

Docker 容器宿主机路径自动检测系统成功解决了容器化环境中路径映射的复杂性问题，实现了真正的"零配置"体验。该系统已经在 rcoder 项目中得到应用，显著简化了部署和配置流程，提高了系统的可用性和可维护性。

通过 Docker API 的巧妙运用和完善的错误处理机制，该系统能够在各种环境中稳定运行，为容器化应用提供了强大的基础设施支持。
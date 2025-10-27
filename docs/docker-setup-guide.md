# RCoder Docker化测试解决方案

## 概述

本解决方案提供了一套完整的Docker化测试环境，用于RCoder项目的开发和测试。该方案避免了重复构建的耗时问题，提供了便捷的一键式管理命令。

## 文件结构

```
rcoder/
├── docker/
│   ├── Dockerfile              # 多阶段构建配置
│   ├── docker-compose.yml      # 容器编排配置
│   ├── start-rcoder.sh         # 容器启动脚本
│   └── scripts/
│       ├── build.sh            # Docker镜像构建脚本
│       └── deploy.sh           # 部署管理脚本
├── Makefile                     # 统一管理命令
└── ... (其他项目文件)
```

## 核心特性

### 1. 多阶段构建优化
- **Builder阶段**: 使用Rust环境编译二进制文件
- **Runtime阶段**: 轻量级Debian镜像运行应用
- **预编译优化**: 避免每次容器启动时重新编译

### 2. 灵活的启动配置
- **Command启动**: 使用`command`替代`entrypoint`，便于调试
- **环境变量配置**: 支持灵活的环境变量配置
- **健康检查**: 内置服务健康检查机制

### 3. 完整的管理命令
- **一键构建**: `make docker-build`
- **一键启动**: `make docker-up`
- **一键测试**: `make docker-test`
- **日志查看**: `make docker-logs`
- **服务重启**: `make docker-restart`
- **资源清理**: `make docker-clean`

## 使用方法

### 快速开始

1. **构建Docker镜像**:
   ```bash
   make docker-build
   ```

2. **启动容器服务**:
   ```bash
   make docker-up
   ```

3. **查看服务状态**:
   ```bash
   make docker-logs
   ```

4. **一键完整测试**:
   ```bash
   make docker-test
   ```

### 详细命令说明

#### Docker相关命令

```bash
# 构建rcoder Docker镜像
make docker-build

# 启动rcoder容器服务
make docker-up

# 停止rcoder容器服务
make docker-down

# 查看容器服务日志
make docker-logs

# 重启rcoder容器服务
make docker-restart

# 清理Docker资源
make docker-clean

# 完整测试流程（构建+启动+健康检查）
make docker-test
```

#### 传统开发命令

```bash
# 安装依赖并构建（release模式）
make build

# 安装依赖并构建（debug模式）
make dev

# 安装到系统
make install

# 卸载
make uninstall
```

## 配置说明

### Docker Compose配置

- **服务端口**: 8086 (可通过环境变量`RCODER_PORT`配置)
- **健康检查**: 自动检测服务状态
- **卷挂载**:
  - Docker socket: `/var/run/docker.sock`
  - 日志目录: `./logs/:/app/logs`
  - 启动脚本: `./start-rcoder.sh:/app/start-rcoder.sh`

### 环境变量

- `RCODER_PORT`: 服务端口 (默认: 8086)
- `RUST_LOG`: 日志级别 (默认: info)
- `DOCKER_SOCKET_PATH`: Docker socket路径
- `RCODER_WORKSPACE`: 工作空间路径

## 测试验证

### 健康检查

服务启动后，可以通过以下方式验证：

```bash
# 检查服务健康状态
curl http://localhost:8086/health

# 查看容器状态
docker-compose -f docker/docker-compose.yml ps
```

### 功能测试

1. **动态容器创建测试**: 测试RCoder创建和管理Docker容器的能力
2. **容器间通信测试**: 验证RCoder与容器内服务的通信
3. **文件系统集成测试**: 验证路径解析和文件访问功能

## 故障排除

### 常见问题

1. **网络连接问题**: 如果遇到Docker镜像拉取超时，请检查网络连接
2. **权限问题**: 确保有访问Docker socket的权限
3. **端口冲突**: 如果8086端口被占用，请修改`docker-compose.yml`中的端口配置

### 日志查看

```bash
# 查看容器日志
make docker-logs

# 查看构建日志
docker-compose -f docker/docker-compose.yml logs rcoder
```

## 开发工作流

### 推荐测试流程

1. **代码修改**: 在宿主机上修改RCoder代码
2. **快速构建**: `make docker-build` - 只构建变化的层
3. **启动测试**: `make docker-up` - 启动容器环境
4. **功能验证**: 测试动态容器创建和通信功能
5. **日志调试**: `make docker-logs` - 查看详细日志
6. **清理重置**: `make docker-clean` - 清理环境

### 性能优化

- **镜像层缓存**: 利用Docker层缓存减少构建时间
- **并行构建**: 多阶段构建支持并行处理
- **增量更新**: 只重新构建变化的层

## 总结

该Docker化测试解决方案提供了：

✅ **高效的开发流程**: 避免重复构建，提升开发效率
✅ **完整的管理工具**: 一键式命令，简化操作
✅ **灵活的配置选项**: 支持不同测试场景
✅ **强大的测试能力**: 支持动态容器创建和通信测试
✅ **便于维护**: 清晰的文件结构和完整的文档

通过这套解决方案，您可以专注于RCoder功能的开发和测试，而无需担心环境配置和部署问题。
# Docker 本地测试配置说明

## 📁 文件说明

### `config.yml`
本地 Docker 容器测试专用配置文件，用于在 docker-compose 启动的容器中测试动态启动子容器。

### 核心配置

#### 镜像配置
所有子容器使用与主容器相同的镜像：`master-rcoder:latest`

```yaml
services:
  rcoder:
    image: "master-rcoder:latest"
    arm64_image: "master-rcoder:latest"
    amd64_image: "master-rcoder:latest"
    default_image: "master-rcoder:latest"
  
  agent-runner:
    image: "master-rcoder:latest"
    arm64_image: "master-rcoder:latest"
    amd64_image: "master-rcoder:latest"
    default_image: "master-rcoder:latest"
```

#### 路径配置
- **项目工作目录**: `/app/project_workspace`（容器内路径）
- **日志目录**: `/app/logs`
- **规范目录**: `/app/specs`

## 🚀 使用方法

### 1. 构建镜像
```bash
make dev-build
```

### 2. 启动容器
```bash
make dev-up
```

### 3. 测试动态容器启动
容器内的 RCoder 服务会读取 `/app/config.yml`，动态启动的子容器会使用相同的 `master-rcoder:latest` 镜像。

### 4. 查看日志
```bash
make dev-logs
```

## 🔄 与生产环境的区别

| 配置项 | 本地测试 | 生产环境 |
|--------|---------|---------|
| **主容器镜像** | `master-rcoder:latest` | `registry.yichamao.com/rcoder:latest-arm64` |
| **子容器镜像** | `master-rcoder:latest` | `registry.yichamao.com/rcoder:latest-arm64` |
| **配置文件** | `docker/config.yml` | `config.yml` (项目根目录) |
| **项目路径** | `/app/project_workspace` | `./project_workspace` |

## 📝 注意事项

1. **镜像一致性**：本地测试时，主容器和子容器使用相同的镜像，确保环境一致
2. **配置隔离**：`docker/config.yml` 仅用于容器内测试，不影响宿主机配置
3. **路径映射**：容器内的路径会自动映射到宿主机的 `docker/project_workspace`
4. **网络模式**：使用 `bridge` 网络模式，容器间可以通过内部网络通信

## 🐛 故障排查

### 子容器无法启动
```bash
# 检查镜像是否存在
docker images | grep master-rcoder

# 如果镜像不存在，重新构建
make dev-build
```

### 配置文件未生效
```bash
# 检查配置文件是否正确挂载
docker exec -it <container_id> cat /app/config.yml

# 检查日志
make dev-logs
```

### 路径映射问题
```bash
# 检查挂载点
docker inspect <container_id> | grep Mounts -A 20
```

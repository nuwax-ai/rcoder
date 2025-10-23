# Docker Manager 自动架构检测

## 概述

rcoder 现在支持**自动检测系统架构**并动态配置 Docker 容器平台，完美解决了之前硬编码 AMD64 架构导致的平台不匹配问题。系统会根据你的运行环境自动选择合适的架构，无需手动配置。

## 新增功能

### 🚀 自动架构检测

- **智能检测**：系统会自动检测当前运行的操作系统和CPU架构
- **零配置**：无需任何手动设置，开箱即用
- **广泛支持**：支持 macOS、Linux、Windows 的 ARM64 和 AMD64 架构

### 🆕 环境变量（可选）

- **`DOCKER_DEFAULT_PLATFORM`**: 手动设置 Docker 容器的默认平台（优先级高于自动检测）

### 🏗️ 配置字段

在 `DockerManagerConfig` 中新增了 `default_platform` 字段：

```rust
pub struct DockerManagerConfig {
    pub docker_host: Option<String>,
    pub default_image: String,
    pub default_platform: String, // 🆕 新增：默认平台配置
    pub default_network_mode: String,
    // ... 其他字段
}
```

## 使用方式

### 1. 🎯 自动检测（推荐）

无需任何配置，系统会自动检测并使用合适的平台：

```bash
# 系统自动检测并使用合适的平台
cargo run --release --bin rcoder
```

**示例**：
- 在 Mac M1/M2 上 → 自动使用 `linux/arm64`
- 在 Intel Mac 上 → 自动使用 `linux/amd64`
- 在 Linux ARM64 上 → 自动使用 `linux/arm64`
- 在 Linux AMD64 上 → 自动使用 `linux/amd64`
- 在 Windows 上 → 自动使用 `linux/amd64`

### 2. 🔧 手动指定平台

如果需要强制使用特定平台，可以通过环境变量覆盖：

### 2. 设置 ARM64 平台

```bash
export DOCKER_DEFAULT_PLATFORM=linux/arm64
cargo run --release --bin rcoder
```

### 3. 设置 AMD64 平台

```bash
export DOCKER_DEFAULT_PLATFORM=linux/amd64
cargo run --release --bin rcoder
```

### 5. 检查当前配置

```bash
# 运行测试脚本查看检测结果
./test_auto_arch_detection.sh

# 手动检查系统架构
uname -m
```

## 🔍 自动检测逻辑

系统按以下优先级自动选择平台：

### 检测映射表

| 操作系统 | CPU架构 | 检测结果 |
|---------|--------|---------|
| macOS | aarch64 (M1/M2) | `linux/arm64` |
| macOS | x86_64 (Intel) | `linux/amd64` |
| Linux | aarch64 | `linux/arm64` |
| Linux | x86_64 | `linux/amd64` |
| Windows | x86_64 | `linux/amd64` |
| 其他 | arm64 | `linux/arm64` |
| 其他 | 其他 | `linux/amd64` (默认) |

### 配置优先级

1. **环境变量**: `DOCKER_DEFAULT_PLATFORM` (如果设置)
2. **自动检测**: 根据上表自动检测
3. **默认回退**: `linux/amd64` (确保兼容性)

## 支持的平台

| 平台字符串 | 架构 | 说明 |
|-----------|------|------|
| `linux/amd64` | AMD64 | 通用 x86_64 架构 |
| `linux/arm64` | ARM64 | ARM 64位架构 |

## 完整环境变量配置

```bash
# Docker 连接配置
export DOCKER_HOST="unix:///var/run/docker.sock"

# 镜像配置
export DEFAULT_DOCKER_IMAGE="registry.yichamao.com/rcoder:latest"
export DOCKER_DEFAULT_PLATFORM="linux/arm64"

# 网络和目录配置
export DOCKER_NETWORK_MODE="bridge"
export DOCKER_WORK_DIR="/app/workspace"

# 容器管理配置
export DOCKER_AUTO_CLEANUP="true"
export DOCKER_CONTAINER_TTL="3600"
```

## Makefile 集成

Makefile 中的 `update-image-tag` 命令现在会根据当前系统架构自动选择合适的镜像：

```bash
make update-image-tag
```

该命令会：
1. 检测当前系统架构（ARM64 或 AMD64）
2. 将对应架构的镜像标记为 `latest`
3. 确保架构匹配

## 问题解决

### 原始问题

```
Docker responded with status code 404: image with reference registry.yichamao.com/rcoder:latest 
was found but its platform (linux/arm64) does not match the specified platform (linux/amd64)
```

### 解决方案

通过以下两种方式解决：

1. **使用 update-image-tag 命令**：
   ```bash
   make update-image-tag  # 自动选择当前系统架构的镜像
   ```

2. **设置环境变量**：
   ```bash
   export DOCKER_DEFAULT_PLATFORM=linux/arm64  # 匹配当前系统架构
   ```

## 🛠️ 新增 API

### DockerUtils 方法

```rust
// 自动检测当前系统架构
pub fn auto_detect_platform() -> String

// 获取最佳平台配置（优先环境变量，否则自动检测）
pub fn get_optimal_platform() -> String

// 检查镜像是否与当前架构兼容
pub fn is_image_compatible_with_current_arch(image_tag: &str) -> bool
```

## 技术实现

### 修改的文件

1. **`crates/docker_manager/src/types.rs`**
   - 在 `DockerManagerConfig` 中添加 `default_platform` 字段

2. **`crates/docker_manager/src/lib.rs`**
   - 添加 `DEFAULT_PLATFORM` 常量

3. **`crates/docker_manager/src/manager.rs`**
   - 使用配置中的平台而非硬编码

4. **`crates/docker_manager/src/utils.rs`**
   - 新增自动架构检测功能
   - 支持从环境变量读取平台配置
   - 添加镜像兼容性检查

5. **`crates/rcoder/src/proxy_agent/docker_agent.rs`**
   - 使用 `DockerUtils::config_from_env()` 加载配置

### 核心检测逻辑

```rust
pub fn auto_detect_platform() -> String {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    
    match (os, arch) {
        ("macos", "aarch64") => "linux/arm64",
        ("linux", "aarch64") => "linux/arm64",
        ("macos", "x86_64") => "linux/amd64",
        ("linux", "x86_64") => "linux/amd64",
        ("windows", "x86_64") => "linux/amd64",
        (_, "arm64") => "linux/arm64",
        _ => "linux/amd64", // 默认回退
    }.to_string()
}
```

### 向后兼容性

- 默认平台仍为 `linux/amd64`，保持向后兼容
- 如果未设置环境变量，使用默认配置
- 现有的 AMD64 环境无需修改配置

## 测试验证

运行测试脚本验证功能：

```bash
./test_platform_config.sh
```

该脚本会测试：
- 默认配置
- ARM64 平台配置
- AMD64 平台配置
- 镜像可用性检查

## 🌟 最佳实践

### 开发环境
- **零配置**：直接运行，系统自动检测架构
- **混合架构**：在 ARM64 和 AMD64 机器间无缝切换

### 生产环境
- **明确指定**：通过环境变量明确指定所需平台
- **稳定优先**：避免依赖自动检测，确保环境一致性

### CI/CD 部署
```bash
# 在构建脚本中明确指定平台
export DOCKER_DEFAULT_PLATFORM=linux/amd64  # 或 linux/arm64
cargo build --release
```

### Docker 部署
```bash
# 通过环境变量传递平台配置
docker run -e DOCKER_DEFAULT_PLATFORM=linux/arm64 -p 8087:8087 rcoder:latest
```

## 故障排除

### 镜像不存在
```bash
# 拉取对应架构的镜像
docker pull registry.yichamao.com/rcoder:latest-arm64
docker pull registry.yichamao.com/rcoder:latest-amd64
```

### 架构不匹配
```bash
# 检查当前架构
uname -m

# 设置对应平台
export DOCKER_DEFAULT_PLATFORM=linux/arm64  # 或 linux/amd64
```

### 验证配置
```bash
# 检查环境变量
env | grep DOCKER_

# 检查镜像架构
docker images --format "table {{.Repository}}:{{.Tag}}\t{{.Size}}" | grep rcoder
```
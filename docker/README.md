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

## 🧪 测试页面

`test-page/` 目录包含 VNC、音频和输入法透传功能的集成测试页面。

### 文件说明

| 文件 | 说明 |
|------|------|
| `vnc-test.html` | 集成测试页面（VNC + 音频 + IME） |
| `opus-decoder.min.js` | Opus 音频解码库（86KB） |

### 功能测试

测试页面支持以下功能：
- **VNC 远程桌面**: 通过 WebSocket 连接到容器的 noVNC 服务
- **音频流播放**: 接收并播放容器的音频输出（Opus 编码）
- **输入法透传**: 使用本地输入法（如中文输入法）输入到远程桌面

### 使用方法

#### 1. 启动本地 HTTP 服务器

```bash
# 进入测试页面目录
cd /Volumes/soddygo/git_work/rcoder/docker/test-page

# 使用 Python 3 启动 HTTP 服务器
python3 -m http.server 8000
```

#### 2. 访问测试页面

在浏览器中打开：
```
http://127.0.0.1:8000/vnc-test.html
```

#### 3. 配置连接参数

页面加载后，填写以下信息：

**RCoder 代理模式**（推荐）:
- RCoder 服务地址: `http://127.0.0.1:8088`
- User ID: `user_123`
- Project ID: 留空或填写实际项目 ID

**直接端口模式**（开发调试）:
- 填写容器端口映射（需要先手动映射端口）

#### 4. 创建测试容器

如果使用代理模式，需要先创建一个 Computer Agent 容器：

```bash
# 发送聊天请求，自动创建容器
curl -X POST http://127.0.0.1:8088/computer/chat \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "user_123",
    "prompt": "hello"
  }'
```

或者通过测试页面的"发送聊天消息"按钮创建。

#### 5. 测试功能

| 功能 | 操作 |
|------|------|
| **VNC 桌面** | 点击"打开 VNC 桌面"按钮 |
| **音频流** | 确保控制台显示"✅ OpusDecoder 加载成功"，然后启动音频 |
| **输入法** | 点击"连接 IME 服务"，在输入框中使用本地输入法 |

### 预期控制台输出

正常加载时应该看到：
```
[Audio] ✅ OpusDecoder 加载成功
[Audio] OpusDecoder: [class OpusDecoder]
[IME] 连接 IME 服务: ws://127.0.0.1:8088/computer/ime/user_123/xxx/connect
[IME] WebSocket 已连接
```

### 故障排查

#### OpusDecoder 加载失败
```
[Audio] ❌ OpusDecoder 加载失败
```
**解决方法**: 确保 `opus-decoder.min.js` 与 `vnc-test.html` 在同一目录

#### IME 连接失败
```
[IME] WebSocket 连接失败
```
**可能原因**:
1. 容器未创建 - 先发送聊天请求创建容器
2. IME 服务未运行 - 检查容器日志
3. 网络连接问题 - 检查 Pingora 代理是否运行

#### VNC 无法连接
```
[VNC] WebSocket 连接失败
```
**可能原因**:
1. 容器未创建或未运行
2. VNC 服务未启动 - 等待容器完全启动
3. 端口配置错误 - 检查 User ID 和 Project ID

### 技术细节

#### 音频编解码
- **编码**: Opus (48kHz, 双声道)
- **传输**: WebSocket 二进制帧
- **解码**: opus-decoder 库 (WebAssembly)

#### IME 透传协议
```json
// 客户端 → 容器
{
  "type": "text",
  "text": "你好，世界",
  "method": "xdotool"
}

// 容器 → 客户端
{
  "status": "success",
  "message": "文本已输入"
}
```

#### VNC 连接
- **协议**: WebSocket (noVNC)
- **容器端口**: 6080
- **代理路径**: `/computer/vnc/{user_id}/{project_id}/vnc.html`


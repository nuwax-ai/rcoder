# Agent Audio & IME 技术设计方案

## 1. 项目背景

### 1.1 问题描述

当前 RCoder 项目的虚拟桌面（noVNC）存在以下问题：

1. **音频缺失**：远程桌面播放视频时没有声音，无法听到音频内容
2. **输入法限制**：无法使用客户端本地输入法（如搜狗输入法）输入中文到远程桌面

### 1.2 现有架构

```
外部客户端（浏览器）
    ↓
RCoder 主容器 (Pingora 代理 + HTTP API)
    ↓ 透明代理
Agent Runner 子容器
    ├── noVNC 服务 (端口 6080)
    ├── 音频流服务 (端口 6090)
    └── IME 输入法服务 (端口 6091)
```

**核心要求**：
- 子容器的端口不对外暴露，所有服务通过 Pingora 代理访问
- 使用 `{user_id}` 和 `{project_id}` 路由到不同的子容器
- 复用现有的 VNC 代理架构和容器 IP 解析机制

---

## 2. 技术方案设计

### 2.1 整体架构

```
┌─────────────────────────────────────────────────────────────┐
│                      客户端浏览器                             │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐    │
│  │ noVNC    │  │ Audio    │  │ IME      │  │ 本地     │    │
│  │ Viewer   │  │ Player   │  │ Client   │  │ 输入法   │    │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘  └────┬─────┘    │
└───────┼─────────────┼─────────────┼─────────────┼──────────┘
        │             │             │             │
        │ WebSocket   │ WebSocket   │ WebSocket   │
        │             │             │             │
┌───────▼─────────────▼─────────────▼─────────────▼──────────┐
│                 RCoder 主容器 (Pingora 代理)                │
│  ┌────────────────────────────────────────────────────┐    │
│  │  Router (matchit)                                   │    │
│  │  - /computer/vnc/{user}/{proj}/{*path}  → VNC      │    │
│  │  - /computer/audio/{user}/{proj}/{*path} → Audio   │    │
│  │  - /computer/ime/{user}/{proj}/{*path}   → IME     │    │
│  └────────────────────────────────────────────────────┘    │
│                         ↓                                    │
│  ┌────────────────────────────────────────────────────┐    │
│  │  VncBackendResolver (容器 IP 解析)                  │    │
│  │  - 查询 Docker 容器 IP                              │    │
│  │  - IP 缓存管理                                      │    │
│  └────────────────────────────────────────────────────┘    │
└──────────────────────┬───────────────────────────────────┘
                       │ 内部网络
┌──────────────────────▼───────────────────────────────────┐
│         Agent Runner 子容器 (per user/project)            │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐   │
│  │ noVNC        │  │ Audio Server │  │ IME Server   │   │
│  │ :6080        │  │ :6090        │  │ :6091        │   │
│  │ WebSocket    │  │ WebSocket    │  │ WebSocket    │   │
│  └──────────────┘  └──────────────┘  └──────────────┘   │
│         ↓                 ↓                 ↓             │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐   │
│  │ VNC Server   │  │ PulseAudio   │  │ xdotool      │   │
│  │ TigerVNC     │  │ pcmflux      │  │ xclip        │   │
│  └──────────────┘  └──────────────┘  └──────────────┘   │
└──────────────────────────────────────────────────────────┘
```

### 2.2 音频流传输方案

#### 2.2.1 容器端实现

**已实现组件**：
- `audio_server.py`：基于 pcmflux 的音频流服务
- PulseAudio 虚拟音频设备：`virtual_speaker.monitor`
- WebSocket 服务端：`ws://0.0.0.0:6089`（音频流）
- HTTP 静态服务：`http://0.0.0.0:6090`（播放器页面）

**工作原理**：
```
应用程序 (Chrome/Firefox 播放视频)
    ↓ 音频输出
PulseAudio 虚拟音频设备 (virtual_speaker.monitor)
    ↓ 音频采集
pcmflux (C++ 库)
    ↓ Opus 编码 (48kHz, 2 channels, 128kbps)
audio_server.py
    ↓ WebSocket (二进制帧: 0x01 + opus_bytes)
客户端浏览器 (Opus 解码 + Web Audio API 播放)
```

**关键配置**：
```python
# audio_server.py
HTTP_PORT = 6090  # 静态文件服务（播放器页面）
WS_PORT = 6089    # WebSocket 音频流
AUDIO_DEVICE = "virtual_speaker.monitor"

# AudioCaptureSettings
sample_rate = 48000
channels = 2
opus_bitrate = 128000
frame_duration_ms = 20
use_vbr = True
use_silence_gate = True  # 跳过静音片段，节省带宽
```

#### 2.2.2 Pingora 代理层设计

**新增路由规则**：

```rust
// crates/rcoder-proxy/src/router.rs

/// 路由类型枚举 - 新增音频代理
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteType {
    VncProxy,
    PortProxy,
    HealthCheck,
    ApiProxy,
    /// 音频流代理: `/computer/audio/{user_id}/{project_id}/{*path}`
    ///
    /// - `user_id`: 用户标识符
    /// - `project_id`: 项目标识符
    /// - `path`: 剩余路径（如 `ws`, `/` 等）
    ///
    /// **目标**: 容器内的音频流服务
    /// - HTTP (端口 6090): 静态文件（播放器页面）
    /// - WebSocket (端口 6089): 音频流
    ///
    /// **示例**:
    /// - `/computer/audio/user_123/proj_456/` → 播放器页面 (HTTP)
    /// - `/computer/audio/user_123/proj_456/ws` → 音频流 (WebSocket)
    AudioProxy,
}

pub fn create_router() -> Result<Router<RouteType>, anyhow::Error> {
    let mut router = Router::new();
    
    // 现有路由...
    
    // ========================================================================
    // 音频流代理路由
    // ========================================================================
    //
    // 路径格式: /computer/audio/{user_id}/{project_id}/{*path}
    //
    // 功能: 将 HTTP 和 WebSocket 请求代理到用户容器的音频流服务
    //
    // 参数:
    // - user_id: 用户标识符，用于查找对应的容器 IP
    // - project_id: 项目标识符
    // - path: 剩余路径
    //   - "/" 或空: 静态播放器页面 (HTTP 6090)
    //   - "ws": 音频流 WebSocket (WS 6089)
    //
    // 示例:
    // - /computer/audio/user_123/proj_456/ -> 容器IP:6090/
    // - /computer/audio/user_123/proj_456/ws -> 容器IP:6089/ws (WebSocket)
    //
    router
        .insert(
            "/computer/audio/{user_id}/{project_id}/{*path}",
            RouteType::AudioProxy,
        )
        .map_err(|e| {
            tracing::error!("❌ [ROUTER] 音频代理路由插入失败: {}", e);
            anyhow::anyhow!("Audio proxy route configuration error: {}", e)
        })?;
    
    Ok(router)
}
```

**代理实现**：

```rust
// crates/rcoder-proxy/src/service.rs

impl PortProxy {
    /// 处理音频流代理请求
    async fn handle_audio_request(
        &self,
        upstream_request: &mut RequestHeader,
        original_uri: &http::Uri,
        params: Params<'_, '_>,
        ctx: &mut TrackingCtx,
    ) -> PingoraResult<()> {
        // 从路径参数中提取 user_id 和 project_id
        let user_id = params.get("user_id").ok_or_else(|| {
            error!("音频路由缺少 user_id 参数");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        let project_id = params.get("project_id").ok_or_else(|| {
            error!("音频路由缺少 project_id 参数");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        // 提取剩余路径
        let remaining_path = params.get("path").unwrap_or("");
        
        // 判断是 WebSocket 音频流还是 HTTP 静态文件
        let (target_port, target_path) = if remaining_path == "ws" || remaining_path.starts_with("ws/") {
            // WebSocket 音频流 (端口 6089)
            (6089_u16, format!("/{}", remaining_path))
        } else {
            // HTTP 静态文件 (端口 6090)
            (6090_u16, format!("/{}", remaining_path))
        };

        // 从缓存中获取容器 IP
        let container_ip = self.vnc_backends.get(user_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| {
                warn!(
                    "❌ [AUDIO] 用户容器不存在: user_id={}, project_id={}",
                    user_id, project_id
                );
                pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
            })?;

        // 记录上下文信息
        ctx.target_port = Some(target_port);
        ctx.upstream_host = Some(format!("{}:{}", container_ip, target_port));

        info!(
            "🎵 [AUDIO] 音频代理: user_id={}, project_id={}, path={}, target={}:{}",
            user_id, project_id, remaining_path, container_ip, target_port
        );

        // 重写 URI
        let new_uri = Self::rewrite_uri(original_uri, target_path)?;
        upstream_request.set_uri(new_uri);

        // 设置通用请求头
        Self::set_common_headers(upstream_request)?;

        // 对于 WebSocket 请求，保持升级头
        // Pingora 会自动处理 WebSocket 升级

        Ok(())
    }

    /// 处理音频流的上游连接
    async fn handle_audio_upstream(
        &self,
        ctx: &TrackingCtx,
        params: Params<'_, '_>,
    ) -> PingoraResult<Box<HttpPeer>> {
        let user_id = params.get("user_id").ok_or_else(|| {
            error!("音频路由缺少 user_id 参数");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        let remaining_path = params.get("path").unwrap_or("");
        
        // 判断目标端口
        let target_port = if remaining_path == "ws" || remaining_path.starts_with("ws/") {
            6089_u16  // WebSocket 音频流
        } else {
            6090_u16  // HTTP 静态文件
        };

        // 获取容器 IP
        let container_ip = self.vnc_backends.get(user_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| {
                warn!("❌ [AUDIO] 容器不存在: user_id={}", user_id);
                pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
            })?;

        // 记录指标
        self.metrics.record_request();
        self.metrics.record_request_port(target_port).await;
        self.metrics.inc_active();

        // 创建 HTTP Peer
        let peer_addr = format!("{}:{}", container_ip, target_port);
        let mut peer = Box::new(HttpPeer::new(
            peer_addr.clone(),
            false, // 不使用 TLS
            "".to_string(),
        ));

        // 配置连接参数
        peer.options.connection_timeout = Some(Duration::from_secs(10));
        peer.options.read_timeout = Some(Duration::from_secs(300)); // WebSocket 长连接
        peer.options.write_timeout = Some(Duration::from_secs(30));
        peer.options.total_connection_timeout = Some(Duration::from_secs(15));

        debug!(
            "🎵 [AUDIO] 连接到音频后端: {} (port={})",
            peer_addr, target_port
        );

        Ok(peer)
    }
}

#[async_trait]
impl ProxyHttp for PortProxy {
    // upstream_request_filter 中添加路由处理
    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        let original_uri = upstream_request.uri.clone();
        let path = original_uri.path();

        let matched = self.router.at(path).map_err(|_| {
            warn!("未匹配到路由: {}", path);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
        })?;

        match matched.value {
            RouteType::VncProxy => {
                self.handle_vnc_request(upstream_request, &original_uri, matched.params, ctx).await?;
            }
            RouteType::AudioProxy => {
                self.handle_audio_request(upstream_request, &original_uri, matched.params, ctx).await?;
            }
            RouteType::PortProxy => {
                self.handle_port_proxy_request(upstream_request, &original_uri, matched.params).await?;
            }
            // ... 其他路由类型
        }

        Ok(())
    }

    // upstream_peer 中添加路由处理
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<Box<HttpPeer>> {
        let req_header = _session.req_header();
        let path = req_header.uri.path();

        let matched = self.router.at(path).map_err(|_| {
            warn!("未匹配到路由: {}", path);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
        })?;

        match matched.value {
            RouteType::VncProxy => self.handle_vnc_upstream(ctx, matched.params).await,
            RouteType::AudioProxy => self.handle_audio_upstream(ctx, matched.params).await,
            RouteType::PortProxy => self.handle_port_proxy_upstream(ctx, matched.params).await,
            // ... 其他路由类型
        }
    }
}
```

#### 2.2.3 客户端集成

**前端使用示例**：

```html
<!-- 音频播放器页面 -->
<!DOCTYPE html>
<html>
<head>
    <title>Remote Desktop Audio</title>
</head>
<body>
    <h1>Remote Desktop Audio Stream</h1>
    <div id="status">连接中...</div>
    <button id="playBtn">开始播放</button>

    <script>
        const userId = 'user_123';
        const projectId = 'proj_456';
        
        // 通过 Pingora 代理连接音频流
        const wsUrl = `ws://${window.location.host}/computer/audio/${userId}/${projectId}/ws`;
        
        let audioContext;
        let ws;

        document.getElementById('playBtn').addEventListener('click', async () => {
            // 初始化 Web Audio API
            audioContext = new AudioContext({sampleRate: 48000});
            
            // 连接 WebSocket
            ws = new WebSocket(wsUrl);
            ws.binaryType = 'arraybuffer';
            
            ws.onopen = () => {
                document.getElementById('status').textContent = '已连接';
            };
            
            ws.onmessage = async (event) => {
                const data = new Uint8Array(event.data);
                
                // 检查协议头 (0x01 表示 Opus 音频)
                if (data[0] === 0x01) {
                    const opusData = data.slice(1);
                    
                    // 解码 Opus (需要 opus-decoder 库)
                    const pcmData = await decodeOpus(opusData);
                    
                    // 播放音频
                    const audioBuffer = audioContext.createBuffer(
                        2, pcmData.length / 2, 48000
                    );
                    // ... 填充 audioBuffer 并播放
                }
            };
            
            ws.onerror = (err) => {
                console.error('WebSocket 错误:', err);
            };
            
            ws.onclose = () => {
                document.getElementById('status').textContent = '已断开';
            };
        });
    </script>
</body>
</html>
```

---

### 2.3 输入法透传方案

#### 2.3.1 容器端实现

**已实现组件**：
- `ime_server.py`：WebSocket 输入法服务
- `xdotool`：X11 自动化工具（模拟键盘输入）
- `xclip`：剪贴板工具（备用方案）

**工作原理**：

```
客户端浏览器
    ↓ 用户使用本地输入法输入中文
JavaScript 监听输入事件 (compositionend)
    ↓ WebSocket 发送 JSON: {"type": "text", "text": "你好"}
ime_server.py (WebSocket Server)
    ↓ 调用 xdotool type 或剪贴板粘贴
X11 服务器
    ↓ 将文本输入到当前焦点窗口
远程桌面应用程序接收文本
```

**关键实现**：

```python
# ime_server.py
IME_PORT = 6091
IME_HOST = '0.0.0.0'

# 消息格式
{
    "type": "text",           # 消息类型
    "text": "你好世界",       # 要输入的文本
    "method": "xdotool"       # 输入方法: xdotool | clipboard
}

# xdotool 输入
subprocess.run([
    'xdotool', 'type', 
    '--clearmodifiers',       # 清除修饰键
    '--delay', '10',          # 字符间延迟 10ms
    '--', text
], env={'DISPLAY': ':0'})

# 备用方案：剪贴板粘贴
# 1. 复制到剪贴板
subprocess.Popen(['xclip', '-selection', 'clipboard'], 
                 stdin=subprocess.PIPE).communicate(text.encode('utf-8'))
# 2. 模拟 Ctrl+V
subprocess.run(['xdotool', 'key', '--clearmodifiers', 'ctrl+v'])
```

#### 2.3.2 Pingora 代理层设计

**新增路由规则**：

```rust
// crates/rcoder-proxy/src/router.rs

pub enum RouteType {
    // ... 现有类型
    
    /// IME 输入法代理: `/computer/ime/{user_id}/{project_id}/{*path}`
    ///
    /// - `user_id`: 用户标识符
    /// - `project_id`: 项目标识符
    /// - `path`: 剩余路径（通常为空或 "ws"）
    ///
    /// **目标**: 容器内的 IME 输入法服务（端口 6091, WebSocket）
    ///
    /// **示例**:
    /// - `/computer/ime/user_123/proj_456/` → WebSocket 升级
    ImeProxy,
}

pub fn create_router() -> Result<Router<RouteType>, anyhow::Error> {
    let mut router = Router::new();
    
    // ... 现有路由
    
    // ========================================================================
    // IME 输入法代理路由
    // ========================================================================
    //
    // 路径格式: /computer/ime/{user_id}/{project_id}/{*path}
    //
    // 功能: 将 WebSocket 请求代理到用户容器的 IME 输入法服务
    //
    // 参数:
    // - user_id: 用户标识符，用于查找对应的容器 IP
    // - project_id: 项目标识符
    // - path: 剩余路径（通常为空）
    //
    // 示例:
    // - /computer/ime/user_123/proj_456/ -> 容器IP:6091/ (WebSocket)
    //
    router
        .insert(
            "/computer/ime/{user_id}/{project_id}/{*path}",
            RouteType::ImeProxy,
        )
        .map_err(|e| {
            tracing::error!("❌ [ROUTER] IME 代理路由插入失败: {}", e);
            anyhow::anyhow!("IME proxy route configuration error: {}", e)
        })?;
    
    Ok(router)
}
```

**代理实现**：

```rust
// crates/rcoder-proxy/src/service.rs

/// IME 输入法服务端口
pub const IME_PORT: u16 = 6091;

impl PortProxy {
    /// 处理 IME 输入法代理请求
    async fn handle_ime_request(
        &self,
        upstream_request: &mut RequestHeader,
        original_uri: &http::Uri,
        params: Params<'_, '_>,
        ctx: &mut TrackingCtx,
    ) -> PingoraResult<()> {
        let user_id = params.get("user_id").ok_or_else(|| {
            error!("IME 路由缺少 user_id 参数");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        let project_id = params.get("project_id").ok_or_else(|| {
            error!("IME 路由缺少 project_id 参数");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        let remaining_path = params.get("path").unwrap_or("");
        let target_path = format!("/{}", remaining_path);

        // 获取容器 IP
        let container_ip = self.vnc_backends.get(user_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| {
                warn!(
                    "❌ [IME] 用户容器不存在: user_id={}, project_id={}",
                    user_id, project_id
                );
                pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
            })?;

        // 记录上下文
        ctx.target_port = Some(IME_PORT);
        ctx.upstream_host = Some(format!("{}:{}", container_ip, IME_PORT));

        info!(
            "⌨️ [IME] 输入法代理: user_id={}, project_id={}, path={}, target={}:{}",
            user_id, project_id, remaining_path, container_ip, IME_PORT
        );

        // 重写 URI
        let new_uri = Self::rewrite_uri(original_uri, target_path)?;
        upstream_request.set_uri(new_uri);

        // 设置通用请求头
        Self::set_common_headers(upstream_request)?;

        Ok(())
    }

    /// 处理 IME 输入法的上游连接
    async fn handle_ime_upstream(
        &self,
        ctx: &TrackingCtx,
        params: Params<'_, '_>,
    ) -> PingoraResult<Box<HttpPeer>> {
        let user_id = params.get("user_id").ok_or_else(|| {
            error!("IME 路由缺少 user_id 参数");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        // 获取容器 IP
        let container_ip = self.vnc_backends.get(user_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| {
                warn!("❌ [IME] 容器不存在: user_id={}", user_id);
                pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
            })?;

        // 记录指标
        self.metrics.record_request();
        self.metrics.record_request_port(IME_PORT).await;
        self.metrics.inc_active();

        // 创建 HTTP Peer
        let peer_addr = format!("{}:{}", container_ip, IME_PORT);
        let mut peer = Box::new(HttpPeer::new(
            peer_addr.clone(),
            false, // 不使用 TLS
            "".to_string(),
        ));

        // 配置连接参数（WebSocket 长连接）
        peer.options.connection_timeout = Some(Duration::from_secs(10));
        peer.options.read_timeout = Some(Duration::from_secs(300));
        peer.options.write_timeout = Some(Duration::from_secs(30));
        peer.options.total_connection_timeout = Some(Duration::from_secs(15));

        debug!("⌨️ [IME] 连接到 IME 后端: {}", peer_addr);

        Ok(peer)
    }
}

// 在 upstream_request_filter 和 upstream_peer 中添加 ImeProxy 分支
#[async_trait]
impl ProxyHttp for PortProxy {
    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        // ...
        match matched.value {
            RouteType::VncProxy => { /* ... */ }
            RouteType::AudioProxy => { /* ... */ }
            RouteType::ImeProxy => {
                self.handle_ime_request(upstream_request, &original_uri, matched.params, ctx).await?;
            }
            // ...
        }
        Ok(())
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<Box<HttpPeer>> {
        // ...
        match matched.value {
            RouteType::VncProxy => self.handle_vnc_upstream(ctx, matched.params).await,
            RouteType::AudioProxy => self.handle_audio_upstream(ctx, matched.params).await,
            RouteType::ImeProxy => self.handle_ime_upstream(ctx, matched.params).await,
            // ...
        }
    }
}
```

#### 2.3.3 客户端集成

**前端实现示例**：

```javascript
// ime_client.js - 输入法客户端封装

class ImeClient {
    constructor(userId, projectId) {
        this.userId = userId;
        this.projectId = projectId;
        this.ws = null;
        this.connected = false;
    }

    connect() {
        const wsUrl = `ws://${window.location.host}/computer/ime/${this.userId}/${this.projectId}/`;
        
        this.ws = new WebSocket(wsUrl);
        
        this.ws.onopen = () => {
            console.log('[IME] 连接成功');
            this.connected = true;
        };
        
        this.ws.onerror = (err) => {
            console.error('[IME] 连接错误:', err);
            this.connected = false;
        };
        
        this.ws.onclose = () => {
            console.log('[IME] 连接关闭');
            this.connected = false;
        };
        
        this.ws.onmessage = (event) => {
            try {
                const response = JSON.parse(event.data);
                if (response.status !== 'ok') {
                    console.error('[IME] 服务器错误:', response.message);
                }
            } catch (e) {
                console.error('[IME] 响应解析失败:', e);
            }
        };
    }

    /**
     * 发送文本到远程桌面
     * @param {string} text - 要输入的文本
     * @param {string} method - 输入方法: 'xdotool' | 'clipboard'
     */
    sendText(text, method = 'xdotool') {
        if (!this.connected) {
            console.warn('[IME] 未连接，无法发送文本');
            return;
        }
        
        const message = JSON.stringify({
            type: 'text',
            text: text,
            method: method
        });
        
        this.ws.send(message);
    }

    disconnect() {
        if (this.ws) {
            this.ws.close();
            this.ws = null;
        }
    }
}

// 使用示例：监听 noVNC 画布的输入事件
function setupImeForNoVNC(userId, projectId) {
    const imeClient = new ImeClient(userId, projectId);
    imeClient.connect();
    
    // 获取 noVNC 的画布元素
    const canvas = document.querySelector('#noVNC_canvas');
    
    // 创建隐藏的输入框用于捕获输入法输入
    const inputProxy = document.createElement('input');
    inputProxy.type = 'text';
    inputProxy.style.position = 'absolute';
    inputProxy.style.opacity = '0';
    inputProxy.style.pointerEvents = 'none';
    document.body.appendChild(inputProxy);
    
    // 当用户点击画布时，聚焦到输入代理框
    canvas.addEventListener('click', () => {
        inputProxy.focus();
    });
    
    // 监听输入法完成事件（用户输入完成一个词组）
    inputProxy.addEventListener('compositionend', (event) => {
        const text = event.data;
        
        if (text && text.length > 0) {
            console.log('[IME] 输入完成:', text);
            imeClient.sendText(text);
            
            // 清空输入框
            inputProxy.value = '';
        }
    });
    
    // 监听普通按键（非输入法输入）
    inputProxy.addEventListener('keydown', (event) => {
        // 对于特殊键（如回车、退格），直接发送到 noVNC
        if (event.key === 'Enter' || event.key === 'Backspace') {
            // 让 noVNC 处理这些特殊键
            event.preventDefault();
            // 这里需要调用 noVNC 的键盘事件处理
        }
    });
    
    return imeClient;
}

// 初始化
const imeClient = setupImeForNoVNC('user_123', 'proj_456');
```

---

## 3. 容器 IP 解析复用

### 3.1 现有机制

RCoder 已经为 VNC 代理实现了容器 IP 解析机制：

```rust
// crates/rcoder-proxy/src/vnc_resolver.rs

pub trait VncBackendResolver: Send + Sync {
    /// 根据 user_id 解析容器 IP
    async fn resolve(&self, user_id: &str) -> Result<VncBackendInfo, VncResolveError>;
    
    /// 检查容器是否存在
    async fn exists(&self, user_id: &str) -> bool;
}

pub struct VncBackendInfo {
    pub container_ip: String,
    pub vnc_port: u16,
    pub is_running: bool,
}
```

### 3.2 复用策略

音频和 IME 代理**直接复用** VNC 的容器 IP 解析机制：

```rust
// crates/rcoder-proxy/src/service.rs

pub struct PortProxy {
    // ...
    /// VNC 后端映射: user_id -> container_ip
    /// 这个映射同时用于 VNC、Audio 和 IME 代理
    vnc_backends: Arc<DashMap<String, String>>,
}

impl PortProxy {
    // 音频和 IME 代理使用相同的容器 IP 查询
    async fn handle_audio_request(&self, ...) -> PingoraResult<()> {
        let container_ip = self.vnc_backends.get(user_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| /* 容器不存在 */)?;
        
        // 使用 container_ip 连接到音频端口 6089/6090
    }
    
    async fn handle_ime_request(&self, ...) -> PingoraResult<()> {
        let container_ip = self.vnc_backends.get(user_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| /* 容器不存在 */)?;
        
        // 使用 container_ip 连接到 IME 端口 6091
    }
}
```

**优势**：
- 无需额外的 IP 解析逻辑
- 保持与 VNC 代理的一致性
- 利用现有的 IP 缓存机制（`DashMap`）

### 3.3 容器 IP 更新机制

当容器重启或 IP 变化时，需要更新缓存：

```rust
// crates/rcoder/src/api/computer_agent.rs

impl PingoraProxyService {
    /// 注册或更新容器的后端 IP（VNC、Audio、IME 共享）
    pub async fn register_vnc_backend(&self, user_id: String, container_ip: String) {
        self.vnc_backends.insert(user_id.clone(), container_ip.clone());
        tracing::info!(
            "✅ [PROXY] 注册容器后端: user_id={}, container_ip={}",
            user_id, container_ip
        );
    }
    
    /// 移除容器的后端 IP
    pub async fn unregister_vnc_backend(&self, user_id: &str) {
        self.vnc_backends.remove(user_id);
        tracing::info!("🗑️ [PROXY] 移除容器后端: user_id={}", user_id);
    }
}
```

---

## 4. 实现步骤

### 4.1 Phase 1: 路由层实现

#### 任务清单

- [ ] **修改 `router.rs`**
  - [ ] 在 `RouteType` 枚举中添加 `AudioProxy` 和 `ImeProxy`
  - [ ] 在 `create_router()` 中注册音频路由：`/computer/audio/{user_id}/{project_id}/{*path}`
  - [ ] 在 `create_router()` 中注册 IME 路由：`/computer/ime/{user_id}/{project_id}/{*path}`
  - [ ] 更新 `get_routes_documentation()` 添加新路由文档

- [ ] **编写单元测试**
  - [ ] 测试音频路由匹配：`test_audio_route_matching()`
  - [ ] 测试 IME 路由匹配：`test_ime_route_matching()`
  - [ ] 测试路由参数提取：`test_audio_ime_parameter_extraction()`

### 4.2 Phase 2: 代理逻辑实现

#### 任务清单

- [ ] **修改 `service.rs` - 音频代理**
  - [ ] 实现 `handle_audio_request()` 方法
  - [ ] 实现 `handle_audio_upstream()` 方法
  - [ ] 在 `upstream_request_filter()` 中添加 `RouteType::AudioProxy` 分支
  - [ ] 在 `upstream_peer()` 中添加 `RouteType::AudioProxy` 分支
  - [ ] 添加音频代理的日志和指标记录

- [ ] **修改 `service.rs` - IME 代理**
  - [ ] 实现 `handle_ime_request()` 方法
  - [ ] 实现 `handle_ime_upstream()` 方法
  - [ ] 在 `upstream_request_filter()` 中添加 `RouteType::ImeProxy` 分支
  - [ ] 在 `upstream_peer()` 中添加 `RouteType::ImeProxy` 分支
  - [ ] 添加 IME 代理的日志和指标记录

- [ ] **添加常量定义**
  - [ ] 定义 `AUDIO_HTTP_PORT = 6090`
  - [ ] 定义 `AUDIO_WS_PORT = 6089`
  - [ ] 定义 `IME_PORT = 6091`

### 4.3 Phase 3: 集成测试

#### 任务清单

- [ ] **容器端测试**
  - [ ] 验证 `audio_server.py` 在容器内正常运行
  - [ ] 验证 `ime_server.py` 在容器内正常运行
  - [ ] 测试容器内服务端口监听状态（6089, 6090, 6091）

- [ ] **代理层测试**
  - [ ] 测试音频 HTTP 请求代理：`curl http://localhost:8087/computer/audio/user_123/proj_456/`
  - [ ] 测试音频 WebSocket 代理：`wscat -c ws://localhost:8087/computer/audio/user_123/proj_456/ws`
  - [ ] 测试 IME WebSocket 代理：`wscat -c ws://localhost:8087/computer/ime/user_123/proj_456/`
  - [ ] 验证容器 IP 解析正确性
  - [ ] 验证 WebSocket 升级成功

- [ ] **端到端测试**
  - [ ] 在浏览器中播放远程桌面音频
  - [ ] 验证音频实时性（延迟 < 500ms）
  - [ ] 在浏览器中使用本地输入法输入中文
  - [ ] 验证中文正确输入到远程桌面应用

### 4.4 Phase 4: 文档和优化

#### 任务清单

- [ ] **更新文档**
  - [ ] 更新 `CLAUDE.md` 添加音频和 IME 架构说明
  - [ ] 更新 API 文档说明新的代理路由
  - [ ] 编写客户端集成示例代码

- [ ] **性能优化**
  - [ ] 音频流延迟优化（目标 < 200ms）
  - [ ] 连接池优化（复用 WebSocket 连接）
  - [ ] 容器 IP 缓存策略优化

- [ ] **错误处理**
  - [ ] 添加容器不存在的友好错误提示
  - [ ] 添加 WebSocket 断开自动重连机制
  - [ ] 添加音频服务不可用的降级处理

---

## 5. 关键技术细节

### 5.1 WebSocket 升级处理

Pingora 自动处理 WebSocket 升级，无需手动处理 `Upgrade` 和 `Connection` 头：

```rust
// Pingora 会自动识别 WebSocket 升级请求
// 只需确保正确设置上游连接参数

peer.options.read_timeout = Some(Duration::from_secs(300));  // 长连接
peer.options.write_timeout = Some(Duration::from_secs(30));
```

### 5.2 音频流协议设计

**协议格式**：

```
帧头: 1 byte
  - 0x01: Opus 音频帧
  - 0x02: 控制消息（预留）
  - 0x03: 心跳消息（预留）

帧体: N bytes
  - Opus 编码的音频数据（PCM -> Opus, 48kHz, 2 channels）
```

**优势**：
- 简单高效，无需复杂解析
- 二进制传输，带宽占用小
- Opus 编码，延迟低（20ms 帧）

### 5.3 输入法透传安全性

**潜在风险**：
- 恶意客户端可能发送恶意命令注入

**防护措施**：
```python
# ime_server.py

def sanitize_text(text: str) -> str:
    """清理文本，防止命令注入"""
    # xdotool type 已经很安全，但仍需验证长度
    if len(text) > 10000:  # 限制最大长度
        raise ValueError("Text too long")
    return text

# 使用 '--' 参数分隔符，防止参数注入
subprocess.run(['xdotool', 'type', '--clearmodifiers', '--', text])
```

### 5.4 容器端口映射策略

**不对外暴露端口**：

```yaml
# docker-compose.yml (正确做法)
services:
  agent-runner:
    image: rcoder-agent-runner:latest
    networks:
      - agent-network
    # ❌ 不暴露端口到宿主机
    # ports:
    #   - "6080:6080"
    #   - "6089:6089"
    #   - "6090:6090"
    #   - "6091:6091"

networks:
  agent-network:
    driver: bridge
```

**通过内部网络访问**：

```rust
// 容器间通过内部 IP 直接通信
let container_ip = "172.18.0.5";  // Docker 分配的内部 IP
let audio_ws_addr = format!("{}:6089", container_ip);
let audio_http_addr = format!("{}:6090", container_ip);
let ime_addr = format!("{}:6091", container_ip);
```

---

## 6. 性能指标

### 6.1 音频流性能目标

| 指标 | 目标值 | 说明 |
|------|--------|------|
| 音频延迟 | < 200ms | 从音频产生到浏览器播放的端到端延迟 |
| 带宽占用 | ~128 Kbps | Opus 编码，48kHz, 2 channels |
| CPU 占用 | < 5% | 容器内 audio_server.py 的 CPU 使用率 |
| 并发连接 | 100+ | 单个 Pingora 实例支持的音频流连接数 |

### 6.2 IME 输入性能目标

| 指标 | 目标值 | 说明 |
|------|--------|------|
| 输入延迟 | < 100ms | 从客户端发送到远程桌面显示的延迟 |
| 吞吐量 | 1000+ 字符/秒 | 输入法透传的最大吞吐量 |
| 可靠性 | 99.9% | 文本传输成功率 |

---

## 7. 风险评估

### 7.1 技术风险

| 风险 | 影响 | 缓解措施 |
|------|------|----------|
| WebSocket 连接不稳定 | 音频/输入中断 | 实现自动重连机制 + 心跳检测 |
| 音频延迟过高 | 用户体验差 | 优化编码参数 + 网络 QoS |
| 容器 IP 变化 | 代理失败 | 实现 IP 缓存自动更新机制 |
| xdotool 输入失败 | 中文输入失效 | 提供剪贴板粘贴备用方案 |

### 7.2 安全风险

| 风险 | 影响 | 缓解措施 |
|------|------|----------|
| 命令注入攻击 | 容器被攻击 | 使用 `--` 参数分隔符 + 输入验证 |
| 未授权访问 | 数据泄露 | 基于 `user_id` 的访问控制 |
| WebSocket 劫持 | 会话劫持 | 使用 HTTPS + 会话令牌验证 |

---

## 8. 未来优化方向

### 8.1 音频质量优化

- **自适应比特率**：根据网络状况动态调整 Opus 比特率
- **多声道支持**：支持 5.1 环绕声
- **音频增强**：噪音抑制、回声消除

### 8.2 输入法增强

- **双向同步**：支持远程桌面的输入状态同步到客户端
- **快捷键透传**：支持 Ctrl+C、Ctrl+V 等快捷键
- **富文本支持**：支持输入带格式的文本

### 8.3 架构优化

- **端口统一**：将 VNC、Audio、IME 合并到单个 WebSocket 连接（多路复用）
- **P2P 模式**：客户端与容器直接建立 WebRTC 连接，降低延迟
- **边缘节点**：部署边缘 Pingora 节点，降低网络延迟

---

## 9. 总结

本技术方案通过以下方式解决了虚拟桌面的音频和输入法问题：

### 9.1 核心特性

✅ **音频流传输**
- 基于 pcmflux + Opus 编码的实时音频流
- 通过 Pingora 代理实现透明转发
- 低延迟（< 200ms）、低带宽（128 Kbps）

✅ **输入法透传**
- 客户端使用本地输入法输入中文
- 通过 WebSocket 发送到容器
- 使用 xdotool 注入到远程桌面

✅ **架构一致性**
- 复用 VNC 代理的容器 IP 解析机制
- 统一的路由规则和代理逻辑
- 完全隐藏子容器端口

### 9.2 实现路径

1. **路由层**：在 `router.rs` 中添加 `AudioProxy` 和 `ImeProxy` 路由
2. **代理层**：在 `service.rs` 中实现音频和 IME 的代理逻辑
3. **容器端**：确保 `audio_server.py` 和 `ime_server.py` 正常运行
4. **客户端**：集成音频播放器和输入法客户端

### 9.3 关键优势

- **零端口暴露**：所有服务通过 Pingora 内部路由访问
- **高性能**：Opus 编码 + WebSocket 长连接
- **低延迟**：音频 < 200ms，输入 < 100ms
- **易维护**：复用现有架构，代码改动最小

---

## 10. 测试页面实现

### 10.1 完整测试页面

为了方便测试 VNC、音频流和输入法透传功能，我们提供了一个集成测试页面：`docker/vnc-test.html`

**功能特性**：
1. **VNC 连接管理**：支持 RCoder 代理模式和直接端口模式
2. **音频流播放**：实时音频流接收和播放，支持音量控制
3. **输入法透传**：使用本地输入法输入中文到远程桌面
4. **可视化反馈**：连接状态显示、音频可视化、IME 状态提示

### 10.2 页面结构

```html
<!doctype html>
<html lang="zh-CN">
<head>
    <meta charset="UTF-8" />
    <title>VNC + 音频 + 输入法 测试页面</title>
    <style>
        /* 样式包含：
         * - 响应式布局
         * - 状态指示器（连接/断开/连接中）
         * - 音频可视化动画
         * - 输入法状态提示
         */
    </style>
</head>
<body>
    <!-- 1. 配置区域 -->
    <div class="header">
        <h1>🔌 VNC + 🎵 音频 + ⌨️ 输入法 测试</h1>
        
        <!-- 模式切换：代理模式 / 直接端口模式 -->
        <div class="mode-toggle">...</div>
        
        <!-- RCoder 代理模式配置 -->
        <div class="config-section proxy-mode">
            <input id="baseUrlInput" placeholder="http://127.0.0.1:8088" />
            <input id="userIdInput" placeholder="user_123" />
            <input id="projectIdInput" placeholder="project_id" />
        </div>
        
        <!-- 直接端口模式配置 -->
        <div class="config-section direct-mode">
            <input id="portInput" placeholder="VNC 端口 (6080)" />
            <input id="audioPortInput" placeholder="音频端口 (6089)" />
            <input id="imePortInput" placeholder="IME 端口 (6091)" />
        </div>
        
        <!-- VNC 连接控制 -->
        <div class="controls">
            <button onclick="connectVNC()">连接 VNC</button>
            <button onclick="disconnectVNC()">断开 VNC</button>
            <div class="status" id="vncStatus">VNC 未连接</div>
        </div>
    </div>
    
    <!-- 2. 音频控制面板 -->
    <div class="feature-panel">
        <h3>🎵 音频流</h3>
        <button onclick="toggleAudio()">启动音频</button>
        <input type="range" id="volumeSlider" min="0" max="100" value="80" />
        <div class="audio-visualizer">
            <!-- 音频可视化条 -->
        </div>
        <div class="status" id="audioStatus">音频未连接</div>
    </div>
    
    <!-- 3. 输入法控制面板 -->
    <div class="feature-panel">
        <h3>⌨️ 输入法透传</h3>
        <button onclick="toggleIME()">启动输入法</button>
        <div class="ime-status">未激活</div>
    </div>
    
    <!-- 4. VNC 显示区域 -->
    <div class="vnc-container">
        <iframe id="vncFrame"></iframe>
    </div>
    
    <!-- 5. 隐藏的输入代理框（用于捕获输入法输入） -->
    <input type="text" id="imeInput" class="ime-input" />
    
    <script>
        // JavaScript 逻辑（详见下文）
    </script>
</body>
</html>
```

### 10.3 核心 JavaScript 实现

#### 10.3.1 音频流连接

```javascript
// 全局状态
let audioContext = null;
let audioWs = null;
let audioConnected = false;
let audioGainNode = null;

// 构建音频 WebSocket URL
function buildAudioWsUrl() {
    const mode = document.querySelector('input[name="connectionMode"]:checked').value;
    
    if (mode === "proxy") {
        // RCoder 代理模式
        const baseUrl = document.getElementById("baseUrlInput").value.trim()
            .replace(/^http/, "ws").replace(/\/+$/, "");
        const userId = document.getElementById("userIdInput").value.trim();
        const projectId = document.getElementById("projectIdInput").value.trim();
        
        return `${baseUrl}/computer/audio/${userId}/${projectId}/ws`;
    } else {
        // 直接端口模式
        const port = document.getElementById("audioPortInput").value.trim();
        return `ws://localhost:${port}/ws`;
    }
}

// 连接音频流
async function connectAudio() {
    const wsUrl = buildAudioWsUrl();
    if (!wsUrl) return;
    
    try {
        // 初始化 Web Audio API
        if (!audioContext) {
            audioContext = new (window.AudioContext || window.webkitAudioContext)({
                sampleRate: 48000
            });
            audioGainNode = audioContext.createGain();
            audioGainNode.gain.value = 0.8;
            audioGainNode.connect(audioContext.destination);
        }
        
        // 恢复 AudioContext（浏览器安全策略要求用户交互）
        if (audioContext.state === "suspended") {
            await audioContext.resume();
        }
        
        // 连接 WebSocket
        audioWs = new WebSocket(wsUrl);
        audioWs.binaryType = "arraybuffer";
        
        audioWs.onopen = () => {
            console.log("[Audio] WebSocket 已连接");
            audioConnected = true;
            updateAudioStatus("connected", "音频已连接");
        };
        
        audioWs.onmessage = async (event) => {
            try {
                const data = new Uint8Array(event.data);
                
                // 检查协议头 (0x01 表示 Opus 音频)
                if (data[0] === 0x01) {
                    const opusData = data.slice(1);
                    // 这里需要 Opus 解码器（如 opus-decoder.js）
                    await playAudioChunk(opusData);
                }
            } catch (err) {
                console.error("[Audio] 处理音频数据失败:", err);
            }
        };
        
        audioWs.onerror = (err) => {
            console.error("[Audio] WebSocket 错误:", err);
            updateAudioStatus("disconnected", "音频连接失败");
        };
        
        audioWs.onclose = () => {
            console.log("[Audio] WebSocket 已关闭");
            audioConnected = false;
            updateAudioStatus("disconnected", "音频已断开");
        };
    } catch (err) {
        console.error("[Audio] 连接失败:", err);
        alert("音频启动失败: " + err.message);
    }
}

// 播放音频块（需要实际的 Opus 解码器）
async function playAudioChunk(opusData) {
    // 注意：这里需要实际的 Opus 解码器
    // 可以使用 opus-decoder.js 或 libopus.js
    
    // 实际实现步骤：
    // 1. 解码 Opus -> Float32Array PCM
    // 2. 创建 AudioBuffer
    // 3. 使用 AudioBufferSourceNode 播放
    
    console.log("[Audio] 收到音频数据:", opusData.length, "bytes");
}

// 设置音量
function setVolume(value) {
    if (audioGainNode) {
        audioGainNode.gain.value = value / 100;
    }
    document.getElementById("volumeValue").textContent = value + "%";
}
```

#### 10.3.2 输入法透传实现

```javascript
// 全局状态
let imeWs = null;
let imeConnected = false;
let imeEnabled = false;

// 构建 IME WebSocket URL
function buildIMEWsUrl() {
    const mode = document.querySelector('input[name="connectionMode"]:checked').value;
    
    if (mode === "proxy") {
        const baseUrl = document.getElementById("baseUrlInput").value.trim()
            .replace(/^http/, "ws").replace(/\/+$/, "");
        const userId = document.getElementById("userIdInput").value.trim();
        const projectId = document.getElementById("projectIdInput").value.trim();
        
        return `${baseUrl}/computer/ime/${userId}/${projectId}/`;
    } else {
        const port = document.getElementById("imePortInput").value.trim();
        return `ws://localhost:${port}/`;
    }
}

// 连接 IME 服务
function connectIME() {
    const wsUrl = buildIMEWsUrl();
    if (!wsUrl) return;
    
    try {
        console.log("连接 IME 服务:", wsUrl);
        
        // 连接 WebSocket
        imeWs = new WebSocket(wsUrl);
        
        imeWs.onopen = () => {
            console.log("[IME] WebSocket 已连接");
            imeConnected = true;
            imeEnabled = true;
            
            // 更新 UI
            document.getElementById("imeBtn").textContent = "停止输入法";
            document.getElementById("imeStatus").textContent = 
                "已激活 - 点击 VNC 区域开始输入";
            
            // 设置输入事件监听
            setupIMEListeners();
        };
        
        imeWs.onmessage = (event) => {
            try {
                const response = JSON.parse(event.data);
                if (response.status === "ok") {
                    console.log("[IME] 文本发送成功");
                } else {
                    console.error("[IME] 服务器错误:", response.message);
                }
            } catch (err) {
                console.error("[IME] 响应解析失败:", err);
            }
        };
        
        imeWs.onerror = (err) => {
            console.error("[IME] WebSocket 错误:", err);
            alert("IME 连接失败，请检查服务是否运行");
        };
        
        imeWs.onclose = () => {
            console.log("[IME] WebSocket 已关闭");
            imeConnected = false;
            imeEnabled = false;
            document.getElementById("imeBtn").textContent = "启动输入法";
        };
    } catch (err) {
        console.error("[IME] 连接失败:", err);
        alert("IME 启动失败: " + err.message);
    }
}

// 设置输入法监听器
function setupIMEListeners() {
    const vncContainer = document.getElementById("vncContainer");
    const imeInput = document.getElementById("imeInput");
    
    // 点击 VNC 区域时聚焦到隐藏输入框
    vncContainer.addEventListener("click", () => {
        if (imeEnabled) {
            imeInput.focus();
            console.log("[IME] 输入框已聚焦");
        }
    });
    
    // 监听输入法完成事件
    imeInput.addEventListener("compositionend", (event) => {
        const text = event.data;
        
        if (text && text.length > 0 && imeConnected) {
            console.log("[IME] 输入完成:", text);
            sendTextToRemote(text);
            
            // 清空输入框
            imeInput.value = "";
        }
    });
}

// 发送文本到远程桌面
function sendTextToRemote(text) {
    if (!imeConnected || !imeWs) {
        console.warn("[IME] 未连接，无法发送文本");
        return;
    }
    
    const message = JSON.stringify({
        type: "text",
        text: text,
        method: "xdotool"  // 或 "clipboard"
    });
    
    imeWs.send(message);
    console.log("[IME] 发送文本:", text);
}
```

### 10.4 使用说明

#### 10.4.1 RCoder 代理模式

1. **配置连接参数**：
   - RCoder 服务地址：`http://127.0.0.1:8088`
   - User ID：`user_123`
   - Project ID：从聊天响应中获取

2. **连接 VNC**：
   - 点击"连接 VNC"按钮
   - 等待 iframe 加载完成

3. **启动音频**：
   - 点击"启动音频"按钮
   - 在远程桌面播放视频/音乐
   - 调节音量滑块控制音量

4. **启动输入法**：
   - 点击"启动输入法"按钮
   - 点击 VNC 画面区域获得焦点
   - 使用本地输入法输入中文
   - 输入完成后自动发送到远程桌面

#### 10.4.2 直接端口模式

适用于容器端口直接映射到宿主机的场景：

1. **查找端口映射**：
   ```bash
   docker port <container_name>
   # 输出示例:
   # 6080/tcp -> 0.0.0.0:50001
   # 6089/tcp -> 0.0.0.0:50002
   # 6091/tcp -> 0.0.0.0:50003
   ```

2. **配置端口**：
   - VNC 端口：`50001`
   - 音频 WebSocket 端口：`50002`
   - IME 端口：`50003`

3. **连接和使用**：同 RCoder 代理模式

### 10.5 故障排查

#### 10.5.1 音频无声

**问题**：音频流连接成功但听不到声音

**排查步骤**：
1. 检查浏览器控制台是否有错误
2. 确认 AudioContext 已恢复（`audioContext.state === "running"`）
3. 检查音量设置（默认 80%）
4. 确认容器内 PulseAudio 服务运行正常
5. 确认 `audio_server.py` 正在采集音频

**解决方案**：
```javascript
// 手动恢复 AudioContext
if (audioContext.state === "suspended") {
    audioContext.resume().then(() => {
        console.log("AudioContext 已恢复");
    });
}
```

#### 10.5.2 输入法无响应

**问题**：输入中文后不显示在远程桌面

**排查步骤**：
1. 确认 IME WebSocket 已连接（查看控制台日志）
2. 确认点击了 VNC 区域获得焦点
3. 确认远程桌面有可输入的窗口（如文本编辑器）
4. 检查容器内 `xdotool` 是否可用：`docker exec <container> which xdotool`

**解决方案**：
```javascript
// 测试发送
sendTextToRemote("测试文本");

// 切换到剪贴板模式
const message = JSON.stringify({
    type: "text",
    text: text,
    method: "clipboard"  // 使用剪贴板粘贴
});
```

#### 10.5.3 WebSocket 连接失败

**问题**：`WebSocket connection failed`

**排查步骤**：
1. 检查 URL 格式是否正确
2. 确认 Pingora 代理服务运行正常
3. 确认容器内服务监听正确端口
4. 检查网络连通性：`curl -v <websocket_url>`

**解决方案**：
```bash
# 检查代理服务状态
curl http://127.0.0.1:8088/health

# 检查容器服务
docker exec <container> netstat -tuln | grep -E "6089|6091"

# 查看代理日志
tail -f /path/to/pingora.log
```

### 10.6 性能优化建议

1. **音频延迟优化**：
   - 使用 `AudioWorklet` 替代 `ScriptProcessorNode`
   - 调整 Opus 帧长度（默认 20ms）
   - 使用 WebAssembly Opus 解码器

2. **输入法响应优化**：
   - 批量发送字符（debounce）
   - 使用二进制协议替代 JSON
   - 预连接 WebSocket

3. **UI 优化**：
   - 使用 CSS `will-change` 优化动画
   - 虚拟化长列表（如日志）
   - 懒加载音频可视化

---

**文档版本**: v1.1  
**更新日期**: 2026-01-05  
**作者**: Claude (Sonnet 4.5)

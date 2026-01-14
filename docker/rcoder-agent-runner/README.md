# RCoder Agent Runner - 远程桌面服务

本目录包含 `rcoder-agent-runner` 容器的配置文件，提供完整的远程桌面解决方案，包括 VNC 远程桌面、本地输入法透传、音频流传输等功能。

## 📋 目录

- [架构概览](#架构概览)
- [服务端口](#服务端口)
- [VNC 远程桌面](#vnc-远程桌面)
- [本地输入法透传](#本地输入法透传)
- [音频流传输](#音频流传输)
- [前端集成指南](#前端集成指南)
- [API 接口文档](#api-接口文档)

---

## 架构概览

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           用户浏览器                                      │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│   ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐     │
│   │ noVNC Canvas     │  │ IME 隐藏输入框    │  │ Audio Player     │     │
│   │ (VNC 画面)        │  │ (输入法捕获)      │  │ (音频播放)        │     │
│   └────────┬─────────┘  └────────┬─────────┘  └────────┬─────────┘     │
│            │                     │                     │               │
│            │ ws://host:6080      │ ws://host:6091      │ ws://host:6089│
│            │ (VNC 协议)           │ (文本输入)           │ (Opus 音频)   │
│            ▼                     ▼                     ▼               │
└────────────┬─────────────────────┬─────────────────────┬───────────────┘
             │                     │                     │
┌────────────┴─────────────────────┴─────────────────────┴───────────────┐
│                           容器内部                                      │
├────────────────────────────────────────────────────────────────────────┤
│                                                                        │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  ┌────────────┐ │
│  │ noVNC Proxy  │  │ IME Server   │  │ Audio Server │  │ x11vnc     │ │
│  │ Port: 6080   │  │ Port: 6091   │  │ Port: 6089   │  │ Port: 5900 │ │
│  └──────────────┘  └──────────────┘  └──────────────┘  └────────────┘ │
│         │                 │                 │                │        │
│         ▼                 ▼                 ▼                ▼        │
│  ┌─────────────────────────────────────────────────────────────────┐  │
│  │                    X11 Display (:0)                             │  │
│  │                    XFCE4 桌面环境                                │  │
│  └─────────────────────────────────────────────────────────────────┘  │
│                                                                        │
└────────────────────────────────────────────────────────────────────────┘
```

---

## 服务端口

| 端口 | 服务 | 协议 | 说明 |
|------|------|------|------|
| **6080** | noVNC | WebSocket (VNC) | 远程桌面画面和键鼠输入 |
| **6091** | IME Server | WebSocket (JSON) | 本地输入法文本透传 |
| **6089** | Audio Server | WebSocket (Binary) | 音频流传输 (Opus 编码) |
| **6090** | Audio HTTP | HTTP | 音频播放器静态页面 |
| 5900 | x11vnc | VNC | 原始 VNC 协议（内部使用） |

---

## VNC 远程桌面

### 功能说明

- **基于 noVNC**: 纯 Web VNC 客户端，无需安装插件
- **分辨率**: 默认 1280x800，可通过 URL 参数调整
- **自动连接**: 支持 `autoconnect=true` 参数
- **自适应缩放**: 支持 `resize=scale` 参数

### 前端接入

#### 方式一：直接嵌入 iframe

```html
<iframe 
    src="http://容器IP:6080/vnc.html?autoconnect=true&resize=scale"
    width="100%" 
    height="600"
    frameborder="0">
</iframe>
```

#### 方式二：使用 noVNC JavaScript API

```html
<script src="http://容器IP:6080/core/rfb.js" type="module"></script>

<div id="vnc-container"></div>

<script type="module">
import RFB from 'http://容器IP:6080/core/rfb.js';

const rfb = new RFB(
    document.getElementById('vnc-container'),
    `ws://容器IP:6080`
);

rfb.scaleViewport = true;
rfb.resizeSession = true;

rfb.addEventListener('connect', () => console.log('VNC connected'));
rfb.addEventListener('disconnect', () => console.log('VNC disconnected'));
</script>
```

### URL 参数

| 参数 | 说明 | 示例 |
|------|------|------|
| `autoconnect` | 自动连接 | `true` |
| `resize` | 缩放模式 | `scale` / `remote` |
| `quality` | 画面质量 | `0-9` (9 最高) |
| `compression` | 压缩级别 | `0-9` (9 最高压缩) |

---

## 本地输入法透传

### 功能说明

允许用户使用**宿主机的输入法**（如搜狗输入法、微软拼音等）直接输入到远程桌面，无需在远程桌面内切换输入法。

### 工作原理

```
1. 用户点击 VNC 画面 → 焦点切换到隐藏输入框
2. 用户使用本地输入法输入（如输入 "zhongguo" 选择 "中国"）
3. 前端捕获 compositionend 事件，获取 "中国"
4. 通过 WebSocket 发送到 IME Server
5. IME Server 使用 xdotool 将文本输入到远程桌面当前焦点窗口
```

### 前端接入

#### 方式一：使用内置脚本（推荐）

如果使用我们提供的 `vnc.html`，IME 透传已内置，无需额外配置。

#### 方式二：手动集成

```html
<!-- 1. 创建隐藏输入框 -->
<textarea id="ime-capture" style="position:fixed;left:-9999px;opacity:0;"></textarea>

<!-- 2. 建立 WebSocket 连接 -->
<script>
const imeWs = new WebSocket('ws://容器IP:6091');

// 连接状态
imeWs.onopen = () => console.log('IME connected');
imeWs.onclose = () => console.log('IME disconnected');
imeWs.onerror = (e) => console.error('IME error:', e);

// 响应处理
imeWs.onmessage = (event) => {
    const data = JSON.parse(event.data);
    if (data.status === 'ok') {
        console.log('Text sent successfully');
    } else if (data.status === 'error') {
        console.error('Error:', data.message);
    }
};

// 3. 监听输入法事件
const imeInput = document.getElementById('ime-capture');

imeInput.addEventListener('compositionend', (e) => {
    if (e.data && imeWs.readyState === WebSocket.OPEN) {
        imeWs.send(JSON.stringify({
            type: 'text',
            text: e.data
        }));
        imeInput.value = '';
    }
});

// 4. VNC 画面点击时聚焦到隐藏输入框
document.getElementById('vnc-container').addEventListener('click', () => {
    imeInput.focus();
});
</script>
```

### WebSocket 协议

#### 发送消息格式

```json
{
    "type": "text",
    "text": "要输入的文本",
    "method": "xdotool"  // 可选: "xdotool"(默认) 或 "clipboard"
}
```

#### 响应消息格式

```json
// 成功
{ "status": "ok" }

// 失败
{ "status": "error", "message": "错误信息" }

// 心跳响应
{ "type": "pong" }
```

#### 心跳保活

```json
// 发送
{ "type": "ping" }

// 响应
{ "type": "pong" }
```

---

## 音频流传输

### 功能说明

将远程桌面的音频（如视频播放、系统提示音）通过 WebSocket 实时传输到浏览器播放。

### 技术栈

- **采集**: PulseAudio 虚拟声卡
- **编码**: Opus (48kHz, 立体声)
- **传输**: WebSocket (二进制)
- **播放**: Web Audio API

### 前端接入

#### 方式一：嵌入音频播放器页面

```html
<iframe 
    src="http://容器IP:6090/index.html"
    width="300"
    height="50"
    frameborder="0">
</iframe>
```

#### 方式二：直接使用 pcmflux 客户端

```html
<!-- 引入 pcmflux 客户端库 -->
<script src="http://容器IP:6090/pcmflux.js"></script>

<button id="audio-toggle">🔊 开启音频</button>

<script>
let audioContext = null;
let audioPlayer = null;

document.getElementById('audio-toggle').addEventListener('click', async () => {
    if (!audioContext) {
        // 初始化 Audio Context (需要用户交互)
        audioContext = new AudioContext({ sampleRate: 48000 });
        
        // 连接 WebSocket
        const ws = new WebSocket('ws://容器IP:6089');
        ws.binaryType = 'arraybuffer';
        
        // 创建 Opus 解码器和播放器
        audioPlayer = new PCMFluxPlayer(audioContext, ws);
        audioPlayer.start();
        
        document.getElementById('audio-toggle').textContent = '🔇 关闭音频';
    } else {
        // 关闭音频
        audioPlayer.stop();
        audioContext.close();
        audioContext = null;
        
        document.getElementById('audio-toggle').textContent = '🔊 开启音频';
    }
});
</script>
```

### WebSocket 协议

音频数据通过 WebSocket 以**二进制帧**传输，格式为 Opus 编码的音频包。

```javascript
const ws = new WebSocket('ws://容器IP:6089');
ws.binaryType = 'arraybuffer';

ws.onmessage = (event) => {
    const opusData = new Uint8Array(event.data);
    // 使用 Opus 解码器解码后播放
    decodeAndPlay(opusData);
};
```

### 注意事项

> [!IMPORTANT]
> **浏览器安全限制**: 由于浏览器自动播放策略，音频播放必须由用户交互（如点击按钮）触发。

---

## 前端集成指南

### 完整示例

```html
<!DOCTYPE html>
<html>
<head>
    <title>远程桌面</title>
    <style>
        body { margin: 0; font-family: Arial, sans-serif; }
        #container { display: flex; flex-direction: column; height: 100vh; }
        #toolbar { padding: 10px; background: #333; color: white; display: flex; gap: 10px; align-items: center; }
        #vnc-wrapper { flex: 1; position: relative; }
        #vnc-container { width: 100%; height: 100%; }
        #ime-capture { position: fixed; left: -9999px; opacity: 0; }
        .status { padding: 5px 10px; border-radius: 4px; font-size: 12px; }
        .connected { background: #4CAF50; }
        .disconnected { background: #f44336; }
    </style>
</head>
<body>
    <div id="container">
        <!-- 工具栏 -->
        <div id="toolbar">
            <span>远程桌面</span>
            <span id="vnc-status" class="status disconnected">VNC: 未连接</span>
            <span id="ime-status" class="status disconnected">输入法: 未连接</span>
            <button id="audio-btn">🔊 开启音频</button>
        </div>
        
        <!-- VNC 画面 -->
        <div id="vnc-wrapper">
            <div id="vnc-container"></div>
        </div>
    </div>
    
    <!-- IME 隐藏输入框 -->
    <textarea id="ime-capture"></textarea>
    
    <script type="module">
        // ========== 配置 ==========
        const HOST = location.hostname || 'localhost';
        const VNC_PORT = 6080;
        const IME_PORT = 6091;
        const AUDIO_PORT = 6089;
        
        // ========== VNC 连接 ==========
        import RFB from `http://${HOST}:${VNC_PORT}/core/rfb.js`;
        
        const rfb = new RFB(
            document.getElementById('vnc-container'),
            `ws://${HOST}:${VNC_PORT}`
        );
        
        rfb.scaleViewport = true;
        rfb.addEventListener('connect', () => {
            document.getElementById('vnc-status').textContent = 'VNC: 已连接';
            document.getElementById('vnc-status').className = 'status connected';
        });
        rfb.addEventListener('disconnect', () => {
            document.getElementById('vnc-status').textContent = 'VNC: 已断开';
            document.getElementById('vnc-status').className = 'status disconnected';
        });
        
        // ========== IME 输入法透传 ==========
        const imeWs = new WebSocket(`ws://${HOST}:${IME_PORT}`);
        const imeInput = document.getElementById('ime-capture');
        
        imeWs.onopen = () => {
            document.getElementById('ime-status').textContent = '输入法: 已连接';
            document.getElementById('ime-status').className = 'status connected';
        };
        imeWs.onclose = () => {
            document.getElementById('ime-status').textContent = '输入法: 已断开';
            document.getElementById('ime-status').className = 'status disconnected';
        };
        
        imeInput.addEventListener('compositionend', (e) => {
            if (e.data && imeWs.readyState === WebSocket.OPEN) {
                imeWs.send(JSON.stringify({ type: 'text', text: e.data }));
                imeInput.value = '';
            }
        });
        
        document.getElementById('vnc-container').addEventListener('click', () => {
            imeInput.focus();
        });
        
        // ========== 音频 ==========
        let audioWs = null;
        document.getElementById('audio-btn').addEventListener('click', () => {
            if (!audioWs) {
                audioWs = new WebSocket(`ws://${HOST}:${AUDIO_PORT}`);
                audioWs.binaryType = 'arraybuffer';
                // 音频处理逻辑...
                document.getElementById('audio-btn').textContent = '🔇 关闭音频';
            } else {
                audioWs.close();
                audioWs = null;
                document.getElementById('audio-btn').textContent = '🔊 开启音频';
            }
        });
    </script>
</body>
</html>
```

---

## API 接口文档

### VNC 服务 (Port 6080)

| 端点 | 说明 |
|------|------|
| `GET /vnc.html` | noVNC 客户端页面 |
| `GET /vnc_lite.html` | noVNC 精简版页面 |
| `GET /core/rfb.js` | noVNC RFB 模块 |
| `WS /` | VNC WebSocket 连接 |

### IME 服务 (Port 6091)

| 端点 | 协议 | 说明 |
|------|------|------|
| `WS /` | WebSocket | 文本输入通道 |

**消息格式**:
```typescript
// 请求
interface IMERequest {
    type: 'text' | 'ping';
    text?: string;          // type='text' 时必填
    method?: 'xdotool' | 'clipboard';  // 可选，默认 xdotool
}

// 响应
interface IMEResponse {
    status?: 'ok' | 'error';
    message?: string;       // status='error' 时的错误信息
    type?: 'pong';          // 心跳响应
}
```

### 音频服务 (Port 6089/6090)

| 端点 | 协议 | 说明 |
|------|------|------|
| `GET :6090/` | HTTP | 音频播放器页面 |
| `GET :6090/pcmflux.js` | HTTP | PCMFlux 客户端库 |
| `WS :6089/` | WebSocket (Binary) | 音频流 (Opus) |

---

## 文件说明

| 文件 | 说明 |
|------|------|
| `Dockerfile` | 容器构建配置 |
| `start-up.sh` | 容器启动脚本，启动所有服务 |
| `ime_server.py` | IME 输入法透传后端服务 |
| `ime_passthrough.js` | IME 输入法透传前端脚本 |
| `audio_server.py` | 音频流传输服务 |
| `audio_static/` | 音频播放器静态文件 |

---

## 常见问题

### Q: 中文输入不生效？
A: 确保点击了 VNC 画面中的文本输入框，且 IME WebSocket 已连接。

### Q: 音频没有声音？
A: 浏览器需要用户交互才能播放音频，请点击"开启音频"按钮。

### Q: VNC 画面模糊？
A: 尝试在 URL 中添加 `?quality=9` 参数提高画质。

### Q: 如何禁用 IME 透传？
A: 启动容器时设置 `ENABLE_IME_PASSTHROUGH=false` 环境变量。

---

## 联系方式

如有问题，请联系后端开发团队。

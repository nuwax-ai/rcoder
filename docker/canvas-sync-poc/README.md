# Canvas 双向同步 POC

验证「宿主机浏览器(viewer)实时看到容器内 Chrome 画面 + 在 viewer 上点击/打字能真实操作容器内 Chrome」这条闭环——用 CDP `Page.startScreencast`(正向画面)+ `Input.dispatchMouseEvent/KeyEvent`(反向输入)替代 noVNC 的整个桌面像素流。

## 架构

```
宿主机浏览器  ──http/ws──▶  容器 (dev-canvas-sync-poc)
  http://localhost:9223/        ├─ chromium --headless --remote-debugging-port=9222
  (viewer: canvas 渲染 + 输入)  └─ node relay (9223): serve viewer + 桥接 CDP
```

- 正向:`Page.startScreencast` → JPEG 帧(binary WS)+ meta/navigate(JSON WS)→ Canvas `drawImage`
- 反向:Canvas 上的 pointer/wheel/key → JSON WS → `Input.dispatchMouseEvent / dispatchKeyEvent / insertText`

## 前置

本地需有 `dev-rcoder-agent-runner:latest` 镜像(复用其 Chromium + Node22):

```bash
docker images | grep dev-rcoder-agent-runner
# 没有则先在 rcoder 根目录: make docker-build-agent-runner
```

## 构建并启动

```bash
cd docker/canvas-sync-poc
docker compose up --build
```

预期日志:
```
[start] chrome ready (probe #N)
[relay] chrome attached; page url = http://127.0.0.1:9223/test-page/index.html
[relay] listening on http://0.0.0.0:9223
```

## 验证(端到端)

宿主机浏览器打开 **http://localhost:9223** ,逐项验证:

| # | 操作 | 预期 | 验证通道 |
|---|------|------|---------|
| 1 | 打开 viewer | canvas 显示测试页(弹框、计数器、输入框、滚动区) | 正向画面 |
| 2 | 在 canvas 上点弹框右上角 ✕ | 弹框消失 | 反向点击(关弹框) |
| 3 | 点「点我 +1」按钮 | 数字递增,画面实时刷新 | 反向点击 + 画面 |
| 4 | 点输入框后打字(英文/数字) | 下方回显文字 | 反向键盘 |
| 5 | 在滚动区上滚轮 | 内容滚动 | 反向滚轮 |
| 6 | 新标签访问 `http://localhost:9223/agent/click?x=300&y=200` | viewer 画面看到容器 Chrome 在该坐标点击 | 模拟 agent 操作被 viewer 看到 |

## 端点

| 路径 | 说明 |
|------|------|
| `GET /` | viewer 页面 |
| `WS /ws` | 双向通道(帧 binary / 控制 JSON / 输入 JSON) |
| `GET /test-page/` | 容器内 Chrome 加载的测试页 |
| `GET /agent/click?x=&y=` | 模拟 agent 点击(方便验证「agent 操作可见」) |

## 协议

`server → client`:
- binary: JPEG 帧
- JSON `{type:'meta', deviceWidth, deviceHeight, pageScaleFactor, offsetTop, url}`(每帧前)
- JSON `{type:'navigate', url}`(导航时)

`client → server`(JSON):
- `{t:'mm'|'mp'|'mr', x, y, btn?, count?, mods}` 鼠标移动/按下/抬起
- `{t:'wh', x, y, dx, dy, mods}` 滚轮
- `{t:'kd'|'ku', key, code, mods}` 按键
- `{t:'ti', text}` 文本输入
- `mods` 位域:`alt=1, ctrl=2, meta=4, shift=8`

## 已知局限(POC v1)

- 单 page、无 tab 跟随
- 无 WS / Chrome 重连(断需重启)
- 无 agent/用户抢占锁(双方可同时操作)
- 本地 http(无 wss),未走 Pingora `/proxy/{port}`
- 中文 IME 组词非实时(可打印字符走 insertText,控制键走 keyDown)
- 文件上传 / 原生拖拽未实现

## 排错

- **`chromium: command not found`**:镜像里命令名可能不同,在 `docker-compose.yml` 加 `environment: - CHROME_BIN=/usr/bin/chromium`。
- **画面不动**:看 `docker logs rcoder-canvas-sync-poc`,通常漏 ack 或 Chrome 没 ready。
- **坐标偏**:viewer 依赖最新 `meta` 做映射,等首帧到达后再操作。
- **点击无反应**:headless Chrome 下确认测试页已加载(`docker exec ... curl http://127.0.0.1:9223/test-page/`)。

## 后续迁移路径

1. **headful**:去掉 `--headless=new`,复用 agent-runner 的 X 桌面(`start-up.sh` 的桌面启动),screencast 帧含真实光标。
2. **接入真实 agent-runner**:把 `relay/viewer-relay.js` 作为 `start-up.sh` 的一个新服务(仿 `start_mcp_proxy_services`),连本机 9222。
3. **Pingora 暴露**:relay 监听 9223,宿主机走 `http://<rcoder-host>/proxy/9223/`(WS 自动透传,见 `crates/rcoder-proxy/src/service/handlers/port_proxy.rs`),即可嵌入 iframe。
4. **抢占锁 v2**:给 `chrome-devtools-mcp` 的 `ToolHandler.handle` 打 patch,工具开始/结束发事件给 relay,relay 切 `agent_working` 状态禁用 viewer 输入。

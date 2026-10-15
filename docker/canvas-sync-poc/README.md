# Canvas 浏览器同步实验

本实验用 CDP `Page.startScreencast` 发送 Chromium 画面，以 `Input` 事件回传鼠标、滚轮和文本。它只验证浏览器画面与输入的交互闭环，使用独立容器，未接入 RCoder 的正式 Computer、生命周期或鉴权链路。正式桌面方案见 [Computer Agent 指南](../../docs/concepts/computer-agent.md)。

## 启动

需要本地 `dev-rcoder-agent-runner:latest` 镜像，构建方法见 [Docker 部署指南](../../docs/deployment/docker.md)。在仓库根目录执行：

```bash
docker compose -f docker/canvas-sync-poc/docker-compose.yml up --build
```

浏览器打开 `http://localhost:9223/`。容器内使用 headless Chromium（内部 CDP 端口 9222）与 Node relay（HTTP/WebSocket 端口 9223），覆盖 Agent Runner 的默认入口，不启动整个桌面。

## 手动检查

| 操作 | 预期 |
|---|---|
| 打开 viewer | Canvas 显示测试页 |
| 关闭弹框、点击计数器 | 测试页状态改变，画面更新 |
| 点击文本框后输入 | 测试页显示输入文本 |
| 在滚动区滚轮 | 内容滚动 |
| 访问 `/agent/click?x=300&y=200` | viewer 看见指定坐标的模拟点击 |

`/agent/click` 是实验用控制端点，不调用真实 AI。此检查不能作为 RCoder 端到端验收。

## 通道与协议

| 端点 | 用途 |
|---|---|
| `GET /` | viewer |
| `WS /ws` | JPEG 画面、元数据与输入事件 |
| `GET /test-page/` | Chromium 测试页 |
| `GET /agent/click?x=&y=` | 模拟点击 |

服务端发送二进制 JPEG 帧，以及 `meta`（设备尺寸、缩放、偏移与 URL）和 `navigate` JSON。客户端发送 `mm/mp/mr` 鼠标、`wh` 滚轮、`kd/ku` 按键、`ti` 文本事件；修饰键位为 `alt=1`、`ctrl=2`、`meta=4`、`shift=8`。具体字段以 [relay](relay/viewer-relay.js) 与 [viewer](public/viewer.js) 为准。

## 限制与排障

目前为单页面实验，无标签跟随、WebSocket/Chrome 重连或 Agent/用户操作互斥；使用本地 HTTP，不经过 Pingora。文件上传、原生拖拽和完整 IME 组词交互尚未实现。

画面不动时检查本实验容器日志和 Chrome 就绪信息；坐标偏移时核对最新元数据与 viewer 缩放。Chromium 命令不存在时根据镜像配置设置 `CHROME_BIN`。只处理本实验容器：

```bash
docker compose -f docker/canvas-sync-poc/docker-compose.yml logs --tail 100
docker compose -f docker/canvas-sync-poc/docker-compose.yml down
```

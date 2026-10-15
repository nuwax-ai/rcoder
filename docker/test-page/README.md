# Computer Agent 手动测试页

`vnc-test.html` 用于手动检查 VNC 画面、Opus 音频和本地输入法文本透传。它连接已有 Computer 容器，不提供聊天或创建容器流程，也不代替自动化 E2E。

## 使用

先按 [Computer Agent 指南](../../docs/concepts/computer-agent.md#创建容器与访问桌面)创建测试容器，并记下相同的 `user_id` 与 `project_id`。从仓库根目录启动静态服务器：

```bash
python3 -m http.server 8000 --bind 127.0.0.1 --directory docker/test-page
```

浏览器打开 `http://127.0.0.1:8000/vnc-test.html`。选择代理模式，填写实际 **Pingora** 地址；默认 Compose 是 `http://127.0.0.1:8089`。页面内置的 8088 初始值不适用于该宿主机映射，必须覆盖。不要填写主 HTTP API 的 8090 入口。User ID 与 Project ID 都必须填写。

也可使用 URL 参数：

```text
http://127.0.0.1:8000/vnc-test.html?mode=proxy&baseUrl=http%3A%2F%2F127.0.0.1%3A8089&userId=test-user&projectId=test-project
```

直接端口模式仅用于已明确映射各服务端口的本地调试。普通 Compose 动态容器不需要额外暴露桌面端口。

## 检查项目

| 检查 | 操作与预期 |
|---|---|
| VNC | 打开远程桌面，确认真实画面更新和键鼠输入；iframe 加载完成提示本身不能证明 VNC 已连接 |
| 音频 | 点击启动音频，远端播放已知音频，本机应能听到；浏览器需先有用户交互 |
| 本地 IME | 先点击远端文本框，再连接 IME，在本地输入中文，确认远端收到完整文本 |
| 分通道错误 | 分别检查 VNC、音频与 IME 的 WebSocket 状态，不用一个通道成功推断其他通道正常 |

音频测试需要同目录的 `opus-decoder.min.js`，请通过 HTTP 访问，避免直接打开 HTML。中文输入取决于远端焦点、UTF-8 环境和字体。延迟与流畅度应记录实际测量条件，不预设保证值。

服务端口、协议和日志位置见 [Computer Agent 指南](../../docs/concepts/computer-agent.md)。完整业务回归入口见 [E2E 说明](../../tests-e2e/tools/README.md)。分享截图或日志前移除内部地址、用户身份及输入内容。

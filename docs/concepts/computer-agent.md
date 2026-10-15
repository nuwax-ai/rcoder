# Computer Agent 桌面、输入法与音频

Computer Agent 在受管理容器中运行 Agent Runner 和桌面环境。普通 Computer 场景按用户复用容器，同一容器可承载多个项目会话；UserApp 的开发容器和工作区有独立生命周期，参见 [UserApp 概念](userapp.md)。

## 入口与服务

控制请求发给 RCoder 主 HTTP 服务，桌面数据经过 Pingora。当前本地 Compose 将主 API 映射到 8090、Pingora 映射到 8089；代理在容器内监听 8088。远端地址以部署配置为准。

```text
浏览器 ── HTTP API/SSE ── RCoder ── gRPC ── Agent Runner ── ACP Agent
浏览器 ── HTTP/WebSocket ── Pingora ── noVNC / IME / 音频服务
                                            │
                                      Xvnc :0 + XFCE
```

| 容器端口 | 服务 | 用途 |
|---|---|---|
| 6080 | noVNC / websockify | 桌面页面、VNC WebSocket |
| 5900 | Xvnc | 内部 VNC，默认显示 `:0`、分辨率 1400×1050 |
| 6091 | IME WebSocket | 本地输入法提交文本 |
| 6089 | 音频 WebSocket | Opus 二进制音频包 |
| 6090 | 音频 HTTP | 播放器静态资源 |
| 7681 | ttyd | Web 终端 |

这些是容器内端口，不代表全部已映射到宿主机。优先使用受管理的代理地址，不公开容器的原始 VNC、CDP 或其他调试端口。

## 创建容器与访问桌面

先启动 [Docker 开发环境](../deployment/docker.md)，再通过幂等的 ensure 接口准备 Computer 容器。它只准备容器，不启动 AI 会话；真实 AI 对话需要另行配置模型并调用 `/computer/chat`。

```bash
RCODER_API_URL=http://127.0.0.1:8090
curl --fail-with-body -X POST "$RCODER_API_URL/computer/pod/ensure"   -H 'Content-Type: application/json'   -d '{"user_id":"test-user","project_id":"test-project","service_type":"computer-agent-runner"}'
```

启用 API Key 时按部署配置附加 `x-api-key`，从本地环境变量读取值。核对接口返回的业务结果，并查询 `/computer/pod/vnc-status` 或观察桌面服务就绪，不能仅凭 HTTP 200 判断画面已可用。请求字段与响应以主服务 `/api/docs/` 为准。

同一测试身份对应的代理路径为：

| 用途 | 路径 |
|---|---|
| noVNC 页面 | `/computer/vnc/test-user/test-project/vnc.html?autoconnect=true&resize=scale` |
| VNC WebSocket | `/computer/vnc/test-user/test-project/websockify` |
| IME WebSocket | `/computer/ime/test-user/test-project/connect` |
| 音频播放器 | `/computer/audio/test-user/test-project/index.html` |
| 音频 WebSocket | `/computer/audio/test-user/test-project/ws` |

例如本地桌面地址为 `http://127.0.0.1:8089/computer/vnc/test-user/test-project/vnc.html?autoconnect=true&resize=scale`。WebSocket 使用同一代理主机，HTTP 对应 `ws`、HTTPS 对应 `wss`。路径中的身份使用实际请求值并正确编码。

主 API 上的 `/computer/desktop/{user_id}/{project_id}` 返回桌面访问信息；主 API 的 `desktop-proxy` 占位接口不会转发桌面。音频和 IME 的同名主 API 路由也只返回代理使用提示。嵌入前端时保留完整代理前缀和 noVNC 的 `websockify` 路径，不将主 API 地址当作桌面上游。

## 两种中文输入方式

远端输入法和本地文本透传是两条不同通道，应分别验证。

远端桌面使用 **fcitx5 进程**。当前启动脚本、镜像与 Chromium launcher 的模块环境为：

```text
DISPLAY=:0
GTK_IM_MODULE=fcitx
QT_IM_MODULE=fcitx
XMODIFIERS=@im=fcitx
INPUT_METHOD=fcitx
```

进程名带 `5`，模块值仍为 `fcitx`。桌面与 Chromium 需要使用同一 D-Bus 会话，启动器从 `/tmp/dbus-session-env` 载入地址。优先使用镜像自带的 Chromium launcher，避免单独启动浏览器时遗漏环境。可在远端文本框中用 `Ctrl+Space` 切换输入法，再实际输入验证。

本地透传由前端捕获 `compositionend` 后发送已提交文本，IME 服务向远端当前焦点窗口输入。必须先点击远端目标文本框；WebSocket 连通不代表焦点正确。手动测试可用[桌面测试页](../../docker/test-page/README.md)。

IME JSON 协议：

```json
{"type":"text","text":"你好","method":"xdotool"}
```

`method` 可使用默认的 `xdotool`，或显式使用 `clipboard`，后者通过剪贴板及粘贴快捷键输入。成功响应为 `{"status":"ok"}`，错误为 `{"status":"error","message":"..."}`；`{"type":"ping"}` 对应 `{"type":"pong"}`。显式设置 `ENABLE_IME_PASSTHROUGH=false` 可关闭该服务。输入文本可能出现在 IME 诊断日志中，分享日志前需脱敏。

## 音频

启动脚本创建 PulseAudio `virtual_speaker`，音频服务采集其 monitor，使用 Opus 48kHz、双声道、20ms 帧，经 WebSocket 二进制包发送。浏览器先解码再通过 Web Audio 播放；需要用户点击开启，不能依赖自动播放。

内置播放器使用 WebCodecs `AudioDecoder`，使用前应核对浏览器支持。手动测试页提供单独的 Opus 解码库。客户端关闭后采集可停止，静音门控也会省略静音数据；无帧时先核对远端实际音频输出。延迟与音质需要在目标环境测量，不提供固定延迟承诺。

## 排障

先确认实际容器及镜像身份，再分通道检查。以下命令中的 `COMPUTER_CONTAINER_ID` 由本环境容器列表选取，不使用其他环境的同名容器。

```bash
docker logs --tail 100 "$COMPUTER_CONTAINER_ID"
docker exec "$COMPUTER_CONTAINER_ID" pgrep -a Xvnc
docker exec "$COMPUTER_CONTAINER_ID" sh -c 'DISPLAY=:0 xdpyinfo >/dev/null'
docker exec "$COMPUTER_CONTAINER_ID" pgrep -a fcitx5
docker exec "$COMPUTER_CONTAINER_ID" supervisorctl status
```

| 症状 | 优先检查 |
|---|---|
| 桌面空白或无法连接 | Xvnc 进程、X11 响应、noVNC 代理和浏览器 WebSocket；日志 `/tmp/xvnc.log`、`/tmp/novnc.log` |
| 远端中文输入失败 | fcitx5、D-Bus 会话、浏览器 launcher 环境；日志 `/tmp/fcitx5-startup.log` |
| 本地 IME 连接或输入失败 | 代理入口、相同用户/项目、远端焦点、服务进程；日志 `${CONTAINER_LOGS_DIR:-/app/container-logs}/ime/ime_server.log` |
| 无声音 | 用户交互、浏览器解码器、远端播放、PulseAudio socket 与虚拟声卡；日志 `/tmp/pulseaudio.log`、`/tmp/audio_server.log` |
| 文件权限错误 | 精确文件的 UID/GID、权限位、挂载来源及部署模式，不递归修改整个 `/home/user` |

Docker 的挂载初始化只修正特定桌面目录；Kubernetes 通过部署的 `fsGroup` 管理工作区权限。需要修正时先定位目标文件和归属，只处理本应用授权的路径，保留数据卷。终端默认用户和工作目录见 [Docker 指南](../deployment/docker.md#工作区与终端)。

启动脚本会检查显示栈并在异常时重建 Xvnc、D-Bus、XFCE 与 noVNC。反复失败时应保留日志、核对镜像和运行配置，不通过删除工作区、锁文件或历史状态绕过恢复。完整回归使用 [E2E 启动器](../../tests-e2e/tools/README.md)，手动页面成功只证明本次所测通道。

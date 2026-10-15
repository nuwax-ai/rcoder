# RCoder Agent Runner 镜像

本目录维护 Computer Agent 镜像及其桌面、音频、输入法和终端启动资源。使用与排障统一见 [Computer Agent 指南](../../docs/concepts/computer-agent.md)，本地构建和更新见 [Docker 部署指南](../../docs/deployment/docker.md)。

| 文件 | 职责 |
|---|---|
| `Dockerfile.base` / `Dockerfile` / `Dockerfile.build` | 系统依赖、最终镜像和 Rust 二进制构建 |
| `start-up.sh` | 桌面及服务编排、就绪检查与显示栈恢复 |
| `start-up-common.sh` / `start-up-*-extra.sh` | Docker 与 Kubernetes 的部署差异 |
| `chromium-browser-launcher.sh` | Chromium 的显示、D-Bus 与输入法环境 |
| `ime_server.py` / `ime_passthrough.js` | 本地输入法文本透传 |
| `audio_server.py` / `audio_static/` | PulseAudio 音频流服务与播放器 |
| `supervisor/` / `ttyd/` | PostgreSQL、dbx 和 Web 终端进程管理 |

从仓库根目录执行 `make docker-build-agent-runner`。更改启动脚本或镜像资源后需要重建镜像并更新目标动态容器；仅重启主服务不会替换已经运行的 Agent 镜像。

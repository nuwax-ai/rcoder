# POC 容器:复用本地已构建的 agent-runner 镜像(含 Chromium + Node22),只覆盖入口
FROM dev-rcoder-agent-runner:latest

WORKDIR /app

# 先装依赖(利用 layer 缓存)
COPY package.json ./
RUN npm install --registry=https://registry.npmmirror.com/

# 拷贝 POC 代码
COPY relay/ ./relay/
COPY public/ ./public/
COPY test-page/ ./test-page/
COPY start.sh ./
RUN chmod +x start.sh

# relay 对外端口(viewer HTTP + /ws WebSocket + /agent/click)
EXPOSE 9223

# 关键:覆盖 agent-runner 默认 ENTRYPOINT(否则会启动整个 XFCE 桌面 + VNC)
ENTRYPOINT []
CMD ["./start.sh"]

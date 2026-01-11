# ============================================================================
# 最终镜像 Dockerfile - rcoder-agent-runner
# ============================================================================
# 此镜像基于基础镜像 rcoder-agent-base:latest
# 只包含业务代码和配置文件，构建速度快
# 构建命令: make docker-build-agent-runner 或 make dev-restart
# ============================================================================

# 使用本地基础镜像
ARG BASE_IMAGE=rcoder-agent-base:latest
FROM ${BASE_IMAGE}

# 设置工作目录
WORKDIR /home/user


# ============================================================================
# 📦 复制业务代码和配置文件
# ============================================================================

# 使用构建参数破坏缓存（每次构建都获取最新版本的工具）
ARG CACHEBUST=1

# 强制将 uv 缓存放在 /root 下，避免被 /home/user 挂载覆盖
ENV UV_CACHE_DIR=/root/.cache/uv
# 强制将 npm 缓存放在 /root 下
ENV npm_config_cache=/root/.npm

# 安装 mcp-stdio-proxy
RUN . /opt/cargo/env && echo "Cache bust: ${CACHEBUST}" && cargo install mcp-stdio-proxy

# 安装 claude-code-acp-rs CLI 工具
RUN . /opt/cargo/env && echo "Cache bust: ${CACHEBUST}" && cargo install claude-code-acp-rs

# 预安装 mcp-server-time (uvx)
RUN uvx mcp-server-time --help || echo "mcp-server-time pre-cached"

# 预安装 chrome-devtools-mcp (npm)
RUN npm install -g chrome-devtools-mcp@latest


# 复制预构建的 agent_runner 二进制文件
COPY bin/agent_runner /usr/local/bin/agent_runner
RUN chmod +x /usr/local/bin/agent_runner

# 复制启动脚本
COPY ./start-up.sh /root/
RUN chmod +x /root/start-up.sh

# 复制中文输入法修复脚本（容器内运行 fix-ime 命令）
COPY ./fix-chrome-ime-final.sh /usr/local/bin/fix-ime
RUN chmod +x /usr/local/bin/fix-ime

# 复制音频服务脚本和静态文件
COPY audio_server.py /usr/local/bin/audio_server.py
COPY audio_static /usr/local/share/audio_static
RUN chmod +x /usr/local/bin/audio_server.py

# 复制 MCP 配置文件（用于容器启动时的共享 MCP 代理服务）
COPY mcp /etc/mcp

# 复制 IME 服务脚本（允许用户使用宿主机输入法输入到远程桌面）
COPY ime_server.py /usr/local/bin/ime_server.py
RUN chmod +x /usr/local/bin/ime_server.py

# ============================================================================
# 🔧 暴露端口
# ============================================================================
# 暴露 agent_runner 端口
EXPOSE 8086

# 暴露音频流端口
EXPOSE 6090
EXPOSE 6089

# 暴露 IME 服务端口
EXPOSE 6091

# ============================================================================
# 🚀 启动配置
# ============================================================================
ENTRYPOINT ["/root/start-up.sh"]

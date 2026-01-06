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

# 安装 mcp-stdio-proxy（频繁更新的工具，放在最终镜像以加快构建）
RUN . /opt/cargo/env && cargo install mcp-stdio-proxy

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

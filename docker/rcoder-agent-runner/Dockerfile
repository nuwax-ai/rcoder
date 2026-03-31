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

# agent-browser 默认配置（独立 Chromium profile，避免与 MCP 锁冲突）
ENV AGENT_BROWSER_EXECUTABLE_PATH=/usr/local/bin/chromium-for-mcp \
    AGENT_BROWSER_PROFILE=/home/user/.config/agent-browser/chromium


# ============================================================================
# 📦 复制业务代码和配置文件
# ============================================================================

# 使用构建参数破坏缓存（每次构建都获取最新版本的工具）
ARG CACHEBUST=1

# 安装 mcp-stdio-proxy（频繁更新的工具，放在最终镜像以加快构建）
# 使用 CACHEBUST 变量触发缓存失效
RUN . /opt/cargo/env && echo "Cache bust: ${CACHEBUST}" && cargo install mcp-stdio-proxy

# 安装 claude-code-acp-rs CLI 工具
RUN . /opt/cargo/env && echo "Cache bust: ${CACHEBUST}" && cargo install claude-code-acp-rs --no-default-features --features otel,sacp-flush

# 安装 agent-browser CLI（仅安装命令行工具，不在构建期下载浏览器）
RUN . /opt/cargo/env && \
    echo "Cache bust: ${CACHEBUST}" && \
    cargo install agent-browser && \
    agent-browser --version

# 测试agent
RUN  echo "Cache bust: ${CACHEBUST}" && npm install -g claude-code-acp-ts@latest


# 安装 nuwaxcode（频繁更新的工具，放在最终镜像以加快构建）
# 使用 CACHEBUST 变量触发缓存失效
# 安装失败时主动退出，避免构建包含不完整的镜像
RUN echo "Cache bust: ${CACHEBUST}" && \
    echo "📦 开始安装 nuwaxcode..." && \
    npm i -g nuwaxcode@latest --registry=https://registry.npmjs.org/ && \
    echo "✅ nuwaxcode 安装完成，开始验证..." && \
    which nuwaxcode && \
    echo "✅ nuwaxcode 验证成功: $(nuwaxcode -v)" || \
    (echo "❌ nuwaxcode 安装或验证失败，构建中断" && exit 1)


# 配置 nuwaxcode - 禁用 websearch, webfetch 和 question 权限
RUN mkdir -p /root/.config/opencode && \
    echo '{\n  "permission": {\n    "websearch": "deny",\n    "webfetch": "deny",\n    "question": "deny"\n  }\n}' > /root/.config/opencode/opencode.json


# 安装 opencode - AI 驱动的代码导航工具
# 注意：npm 包不支持 ARM64，使用官方安装脚本
RUN echo "📦 安装 opencode..." && \
    curl -fsSL https://opencode.ai/install | bash && \
    ln -sf /root/.opencode/bin/opencode /usr/local/bin/opencode && \
    echo "验证 opencode 安装..." && \
    opencode --version && \
    echo "✅ opencode 安装成功（已创建符号链接到 /usr/local/bin）"

# ============================================================================
# 🔧 eBPF 诊断工具安装
# 注意：这些工具会增加镜像大小 (~500MB)
# 通过 make dev-restart 默认启用
# ============================================================================
ARG INSTALL_EBPF_TOOLS=false

RUN if [ "$INSTALL_EBPF_TOOLS" = "true" ]; then \
    echo "🔧 开始安装 eBPF 诊断工具..."; \
    apt-get update && \
    echo "📦 安装 bpftrace（核心 eBPF 诊断工具）..."; \
    apt-get install -y bpftrace && \
    echo "📦 安装 strace（系统调用追踪）..."; \
    apt-get install -y strace && \
    echo "📦 安装 sysstat（性能监控：iostat, mpstat 等）..."; \
    apt-get install -y sysstat && \
    echo "📦 安装 jq（JSON 处理）..."; \
    apt-get install -y jq && \
    rm -rf /var/lib/apt/lists/*; \
    echo "✅ eBPF 诊断工具安装完成"; \
    which bpftrace && bpftrace --version || echo "❌ bpftrace 未安装"; \
    else \
    echo "⚠️  eBPF 工具未安装（生产模式）"; \
    fi

# 安装 FlameGraph 工具
RUN if [ "$INSTALL_EBPF_TOOLS" = "true" ]; then \
    git clone https://github.com/brendangregg/FlameGraph /tmp/FlameGraph \
    && cp /tmp/FlameGraph/*.pl /usr/local/bin/ \
    && rm -rf /tmp/FlameGraph \
    && echo "✅ FlameGraph 工具已安装"; \
    fi

# ============================================================================
# 📊 Pyroscope Agent - 持续剖析（已废弃，保留用于兼容性）
# ============================================================================
ARG INSTALL_PYROSCOPE=false
RUN if [ "$INSTALL_PYROSCOPE" = "true" ]; then \
    echo "📦 安装 Pyroscope Agent..."; \
    apt-get update && \
    apt-get install -y curl ca-certificates && \
    echo "下载 Pyroscope Agent (v1.17.0)..." && \
    curl -fsSL "https://github.com/grafana/pyroscope/releases/download/v1.17.0/pyroscope_1.17.0_linux_arm64.tar.gz" \
    -o /tmp/pyroscope.tar.gz && \
    echo "解压 Pyroscope Agent..." && \
    tar -xzf /tmp/pyroscope.tar.gz -C /usr/local/bin/ && \
    rm /tmp/pyroscope.tar.gz && \
    chmod +x /usr/local/bin/pyroscope && \
    echo "验证 Pyroscope Agent..." && \
    pyroscope --version || echo "⚠️  Pyroscope 安装失败"; \
    else \
    echo "⚠️  Pyroscope 未安装"; \
    fi

# ============================================================================
# 📊 Grafana Alloy - 持续性能数据采集（替代已废弃的 Pyroscope Agent）
# ============================================================================
ARG INSTALL_ALLOY=false
ARG ALLOY_VERSION=v1.12.2
RUN if [ "$INSTALL_ALLOY" = "true" ]; then \
    echo "📦 安装 Grafana Alloy ${ALLOY_VERSION}..."; \
    apt-get update && \
    apt-get install -y wget ca-certificates && \
    echo "检测系统架构..."; \
    ARCH=$(dpkg --print-architecture); \
    echo "系统架构: ${ARCH}"; \
    ALLOY_VER=${ALLOY_VERSION}; \
    ALLOY_VER_NUM=${ALLOY_VER#v}; \
    echo "下载 Alloy deb 包 (${ARCH})..."; \
    wget -qO- "https://github.com/grafana/alloy/releases/download/${ALLOY_VER}/alloy-${ALLOY_VER_NUM}-1.${ARCH}.deb" > /tmp/alloy.deb && \
    echo "下载完成，文件大小: $(ls -lh /tmp/alloy.deb | awk '{print $5}')"; \
    echo "安装 Alloy..."; \
    dpkg -i /tmp/alloy.deb && \
    apt-get install -y -f && \
    rm /tmp/alloy.deb && \
    echo "验证 Alloy 安装..."; \
    alloy --version; \
    else \
    echo "⚠️  Alloy 未安装"; \
    fi

# ============================================================================
# 🔍 bpfcc-tools - Off-CPU 阻塞分析工具
# ============================================================================
RUN if [ "$INSTALL_EBPF_TOOLS" = "true" ]; then \
    echo "📦 安装 bpfcc-tools (包含 offcputime)..."; \
    apt-get update && \
    apt-get install -y bpfcc-tools && \
    rm -rf /var/lib/apt/lists/* && \
    which offcputime-bpfcc && offcputime-bpfcc --help | head -3 || echo "⚠️  offcputime-bpfcc 安装失败"; \
    fi

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
# 🔧 eBPF 工具目录
# ============================================================================
# 复制整个 eBPF 工具目录到容器中
COPY ebpf-tools /usr/local/bin/ebpf-tools
RUN chmod +x /usr/local/bin/ebpf-tools/*.sh && \
    ls -la /usr/local/bin/ebpf-tools/ && \
    echo "✅ eBPF 工具目录已创建: /usr/local/bin/ebpf-tools/"

# 创建便捷的快捷命令（包装脚本）
RUN echo '#!/bin/bash\n/usr/local/bin/ebpf-tools/diag-tool.sh offcpu "$@"' > /usr/local/bin/e-offcpu && \
    echo '#!/bin/bash\n/usr/local/bin/ebpf-tools/diag-tool.sh flame "$@"' > /usr/local/bin/e-flame && \
    echo '#!/bin/bash\n/usr/local/bin/ebpf-tools/diag-tool.sh profile "$@"' > /usr/local/bin/e-profile && \
    echo '#!/bin/bash\n/usr/local/bin/ebpf-tools/diag-tool.sh all "$@"' > /usr/local/bin/e-all && \
    echo '#!/bin/bash\n/usr/local/bin/ebpf-tools/auto-flamegraph.sh start' > /usr/local/bin/e-auto-flame && \
    echo '#!/bin/bash\n/usr/local/bin/ebpf-tools/offcpu-monitor.sh start' > /usr/local/bin/e-offcpu-monitor && \
    echo '#!/bin/bash\n/usr/local/bin/ebpf-tools/syscall-monitor.sh start' > /usr/local/bin/e-syscall-monitor && \
    chmod +x /usr/local/bin/e-offcpu /usr/local/bin/e-flame /usr/local/bin/e-profile /usr/local/bin/e-all /usr/local/bin/e-auto-flame /usr/local/bin/e-offcpu-monitor /usr/local/bin/e-syscall-monitor && \
    echo "✅ eBPF 诊断快捷命令已创建:" && \
    echo "   - e-offcpu, e-flame, e-profile, e-all (手动 CPU 诊断)" && \
    echo "   - e-auto-flame (自动火焰图生成)" && \
    echo "   - e-offcpu-monitor (Off-CPU 阻塞监控)" && \
    echo "   - e-syscall-monitor (系统调用监控)"

# ============================================================================
# 📊 Grafana Alloy 配置文件
# ============================================================================
# 复制 Alloy 配置文件到容器（从 ebpf-tools 目录统一管理）
COPY ebpf-tools/alloy-config.alloy /etc/alloy/config.alloy

# 验证 Alloy 配置文件语法
RUN if command -v alloy >/dev/null 2>&1; then \
    echo "验证 Alloy 配置文件..." && \
    alloy validate /etc/alloy/config.alloy || echo "⚠️  Alloy 配置验证失败"; \
    else \
    echo "⚠️  Alloy 未安装，跳过配置验证"; \
    fi

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

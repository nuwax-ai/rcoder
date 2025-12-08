# 第一阶段：构建 Rust 项目
FROM node:22 AS rust-builder

# 设置环境变量避免交互式提示
ENV DEBIAN_FRONTEND=noninteractive
ENV TZ=Asia/Shanghai

# 设置 ARG TARGETARCH
ARG TARGETARCH

# 设置工作目录
WORKDIR /build

# 安装构建依赖（包括cmake和protoc）
RUN apt-get update && apt-get install -y \
    build-essential \
    cmake \
    pkg-config \
    protobuf-compiler \
    libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/*

# 安装 Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

# 复制整个项目源代码（通过 .dockerignore 排除不需要的文件）
COPY . .

# 构建所有二进制文件（包括 rcoder 和 agent_runner）
RUN cargo build --release --bin rcoder --bin agent_runner

# 第二阶段：运行阶段
FROM node:22

# 设置环境变量
ENV TZ=Asia/Shanghai

# 安装必要的运行时依赖（apt 包管理器）
RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    wget \
    gnupg \
    unzip \
    zip \
    tzdata \
    lsof \
    iproute2 \
    net-tools \
    nginx \
    vim \
    && rm -rf /var/lib/apt/lists/* \
    && ln -snf /usr/share/zoneinfo/$TZ /etc/localtime \
    && echo $TZ > /etc/timezone

# 配置 npm 使用国内镜像源
RUN npm config set registry https://registry.npmmirror.com

# 安装 pnpm
RUN npm install -g pnpm

# 配置 pnpm 使用国内镜像源
RUN pnpm config set registry https://registry.npmmirror.com

# 创建 pnpm 全局存储目录
RUN mkdir -p /app/project_workspace/.pnpm-store

# 配置 pnpm 使用新的存储路径
RUN pnpm config set store-dir /app/project_workspace/.pnpm-store

# 配置 yarn 使用国内镜像源
RUN yarn config set registry https://registry.npmmirror.com

# 安装 vite（最新版本）
RUN npm install -g vite@latest

# 安装额外的命令工具（强制更新到最新版本）
RUN npm install -g @zed-industries/claude-code-acp@latest

# 安装 Claude CLI 工具（强制更新到最新版本）
RUN npm install -g @anthropic-ai/claude-code@latest

# 安装 OpenAI Codex（强制更新到最新版本）
RUN npm install -g @openai/codex@latest

# 安装 Bun
RUN curl -fsSL https://bun.sh/install | bash

# 添加 Bun 到 PATH
ENV PATH="/root/.bun/bin:${PATH}"

# 配置 Bun 使用国内镜像
RUN echo '[install]' > /root/.bunfig.toml && \
    echo 'registry = "https://registry.npmmirror.com"' >> /root/.bunfig.toml

# 安装 uv
RUN curl -LsSf https://astral.sh/uv/install.sh | sh

# 添加uv到PATH
ENV PATH="/root/.local/bin:${PATH}"

# 设置 uv 镜像加速地址
ENV UV_INDEX_URL=https://mirrors.aliyun.com/pypi/simple

# 设置工作目录
WORKDIR /app

# 创建 bin 目录并复制构建产物
RUN mkdir -p /app/bin
COPY --from=rust-builder /build/target/release/rcoder /app/bin/rcoder
COPY --from=rust-builder /build/target/release/agent_runner /app/bin/agent_runner

# 预安装 MCP 工具到 uv 缓存中
RUN uv tool install mcp-server-fetch

# 预安装 context7 MCP 工具到 bun 全局缓存
RUN bun add -g @upstash/context7-mcp

# 创建 Claude Code 配置目录并禁用 webfetch 功能，添加 MCP 工具
RUN mkdir -p /root/.claude && \
    echo '{"permissions":{"deny":["WebFetch", "WebSearch"]},"mcpServers":{"mcp-fetch":{"args":["mcp-server-fetch"],"command":"uvx"},"context7":{"args":["-y","@upstash/context7-mcp"],"command":"bunx","env":{}}}}' > /root/.claude/settings.json

# 只有在二进制文件存在时才复制（避免feature构建时的错误）
RUN if [ -f /build/target/release/codex-acp-agent ]; then \
    cp /build/target/release/codex-acp-agent /app/bin/codex-acp-agent; \
    fi


# 复制 subapp-deployer 源代码（如果存在）
# COPY code/subapp-deployer ./subapp-deployer

# 在 subapp-deployer 目录下安装依赖（如果存在）
# WORKDIR /app/subapp-deployer
# RUN pnpm install

# 设置工作目录
WORKDIR /app

# 设置执行权限
RUN chmod +x /app/bin/rcoder /app/bin/agent_runner

# 将 bin 目录添加到 PATH
ENV PATH="/app/bin:${PATH}"


# 清理默认nginx配置以避免冲突
RUN rm -f /etc/nginx/sites-enabled/default && \
    rm -f /etc/nginx/conf.d/default.conf

# 暴露端口
EXPOSE 8086
EXPOSE 60000
EXPOSE 80

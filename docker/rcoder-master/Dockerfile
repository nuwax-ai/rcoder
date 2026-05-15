# ============================================================================
# 最终镜像 Dockerfile - master-rcoder
# ============================================================================
# 此 Dockerfile 使用多阶段构建：
# 1. 第一阶段：在 node:22 中构建 Rust 项目
# 2. 第二阶段：基于 master-rcoder-base 基础镜像运行
# ============================================================================

# 全局 ARG 声明（必须在第一个 FROM 之前，才能在后续 FROM 中使用）
ARG BASE_IMAGE=master-rcoder-base:latest

# ============================================================================
# 第一阶段：构建 Rust 项目
# ============================================================================
FROM node:22 AS rust-builder

# 设置环境变量避免交互式提示
ENV DEBIAN_FRONTEND=noninteractive
ENV TZ=Asia/Shanghai

# 设置 ARG TARGETARCH
ARG TARGETARCH

# 设置工作目录
WORKDIR /build

# 使用 LinuxMirrors 一键配置阿里云镜像源
# 参考: https://github.com/SuperManito/LinuxMirrors
RUN curl -sSL https://linuxmirrors.cn/main.sh | bash -s -- \
    --source mirrors.aliyun.com \
    --protocol https \
    --use-intranet-source false \
    --install-epel false \
    --backup false \
    --upgrade-software false \
    --clean-cache true \
    --ignore-backup-tips

# 安装构建依赖（包括cmake和protoc）
RUN apt-get update && apt-get install -y \
    build-essential \
    cmake \
    pkg-config \
    protobuf-compiler \
    libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/*

# 安装 Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && \
    . /root/.cargo/env && cargo --version
ENV PATH="/root/.cargo/bin:${PATH}"

# 复制整个项目源代码（通过 .dockerignore 排除不需要的文件）
COPY . .

# 🔧 支持 CARGO_FLAGS 参数，允许外部传递 feature flags
ARG CARGO_FLAGS=""
RUN echo "🔧 Cargo flags: ${CARGO_FLAGS}" && \
    cargo build --release --bin rcoder --bin agent_runner ${CARGO_FLAGS}

# ============================================================================
# 第二阶段：运行阶段（基于基础镜像）
# ============================================================================
# 重新声明 ARG（FROM 之后需要重新声明才能使用）
ARG BASE_IMAGE
FROM ${BASE_IMAGE}

# 设置工作目录
WORKDIR /app

# 安装 git（用于 nuwaxcode 源码安装）
RUN apt-get update && apt-get install -y \
    git \
    && rm -rf /var/lib/apt/lists/*

# 复制 .npmrc 文件
COPY docker/rcoder-master/.npmrc /root/.npmrc

# 创建 bin 目录并复制构建产物
RUN mkdir -p /app/bin
COPY --from=rust-builder /build/target/release/rcoder /app/bin/rcoder
COPY --from=rust-builder /build/target/release/agent_runner /app/bin/agent_runner

# 只有在二进制文件存在时才复制（避免feature构建时的错误）
RUN if [ -f /build/target/release/codex-acp-agent ]; then \
    cp /build/target/release/codex-acp-agent /app/bin/codex-acp-agent; \
    fi

# 设置执行权限
RUN chmod +x /app/bin/rcoder /app/bin/agent_runner



# 使用构建参数破坏缓存（每次构建都获取最新版本的工具）
ARG CACHEBUST=1

# agent install
RUN  echo "Cache bust: ${CACHEBUST}" && npm install -g claude-code-acp-ts@latest

# 安装 codex-acp（Codex ACP 代理）
RUN echo "Cache bust: ${CACHEBUST}" && npm install -g @zed-industries/codex-acp

# # 安装 claude-code-acp-ts（从源码构建，用于测试）
# # 使用 npm pack + npm install -g <tarball> 确保复制文件而非 symlink
# RUN echo "Cache bust: ${CACHEBUST}" && \
#     rm -rf /tmp/claude-code-acp-ts && \
#     git clone -b feat/claude-code-acp-ts https://github.com/nuwax-ai/claude-code-acp-ts.git /tmp/claude-code-acp-ts && \
#     cd /tmp/claude-code-acp-ts && npm install && npm run build && \
#     npm pack . && \
#     npm install -g ./claude-code-acp-ts-*.tgz && \
#     echo "验证 claude-code-acp-ts 安装..." && \
#     (which claude-code-acp-ts > /dev/null 2>&1 && echo "✅ claude-code-acp-ts 安装验证通过") || (echo "❌ claude-code-acp-ts 未正确安装" && exit 1)


# 安装 nuwaxcode（频繁更新的工具，放在最终镜像以加快构建）
# 使用 CACHEBUST 变量触发缓存失效
RUN echo "Cache bust: ${CACHEBUST}" && npm i -g nuwaxcode@latest

# 安装 nuwax-file-server（文件服务器）
RUN echo "Cache bust: ${CACHEBUST}" && npm i -g nuwax-file-server@latest




# 使用国内镜像源加速下载
#ENV RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static/
#ENV RUSTUP_UPDATE_ROOT=https://mirrors.ustc.edu.cn/rust-static/rustup/
# 安装 Rust 并验证
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && \
    echo 'export PATH="/root/.cargo/bin:$PATH"' >> /root/.bashrc && \
    export PATH="/root/.cargo/bin:$PATH" && \
    which cargo && cargo --version

# 设置 PATH 环境变量（确保后续命令能找到 cargo）
ENV PATH="/root/.cargo/bin:${PATH}"

# 复制 cargo 镜像配置文件（使用国内镜像加速）
COPY .cargo/config.toml /root/.cargo/config.toml
# 根据codex-acp改造的codex cli,使用cli是: nuwax-codex-acp
RUN cargo install --git https://github.com/nuwax-ai/codex-acp.git --branch dev



# 配置 nuwaxcode - 禁用 websearch, webfetch 和 question 权限
RUN mkdir -p /root/.config/opencode && \
    echo '{\n  "permission": {\n    "websearch": "deny",\n    "webfetch": "deny",\n    "question": "deny"\n  }\n}' > /root/.config/opencode/opencode.json



# 暴露端口
EXPOSE 8086
EXPOSE 60000
EXPOSE 80

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

# 安装 nuwaxcode（频繁更新的工具，放在最终镜像以加快构建）
# 使用 CACHEBUST 变量触发缓存失效
RUN echo "Cache bust: ${CACHEBUST}" && npm i -g nuwaxcode@latestt --registry=https://registry.npmjs.org/

# 配置 nuwaxcode - 禁用 websearch 和 webfetch 权限
RUN mkdir -p /root/.config/opencode && \
    echo '{\n  "permission": {\n    "websearch": "deny",\n    "webfetch": "deny"\n  }\n}' > /root/.config/opencode/opencode.json

# 暴露端口
EXPOSE 8086
EXPOSE 60000
EXPOSE 80

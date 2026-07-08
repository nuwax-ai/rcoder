# ============================================================================
# 最终镜像 Dockerfile - master-rcoder
# ============================================================================
# 此 Dockerfile 使用多阶段构建：
# 1. 第一阶段：在 node:22 中构建 Rust 项目
# 2. 第二阶段：基于 master-rcoder-base 基础镜像运行
# ============================================================================

# 全局 ARG 声明（必须在第一个 FROM 之前，才能在后续 FROM 中使用）
ARG BASE_IMAGE=dev-master-rcoder-base:latest

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
# 参考: https://github.com/SuperMantero/LinuxMirrors
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

# 使用国内镜像加速 rustup。实测 rsproxy.cn（字节跳动）≈26 MB/s，远快于 USTC 此刻 ≈0.1 MB/s。
# rsproxy 同时提供 rustup dist 与 cargo registry。
ENV RUSTUP_DIST_SERVER=https://rsproxy.cn
ENV RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup

# 安装 Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && \
    . /root/.cargo/env && cargo --version
ENV PATH="/root/.cargo/bin:${PATH}"

# builder 阶段 cargo 镜像（仅作用于 builder，不污染宿主 .cargo/config.toml）
# 加速依赖下载；仓库根 .cargo/config.toml 当前为空（直连 crates.io 很慢）
RUN mkdir -p /root/.cargo && \
    printf '[source.crates-io]\nreplace-with = "rsproxy-sparse"\n[source.rsproxy-sparse]\nregistry = "sparse+https://rsproxy.cn/index/"\n' \
    > /root/.cargo/config.toml

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

WORKDIR /app

# ============================================================================
# ⚠️ 分层顺序原则（重要）：
#   代码无关的静态步骤（apt/git/rustup/npm 工具/脚本 COPY）放前面 → 永久缓存；
#   代码产物 COPY --from=rust-builder 放**最后** → 代码改动只让最后几步重跑。
#   此前 rustup 排在 COPY-from-builder 之后，导致每次 make dev-restart 都重下
#   整个 Rust toolchain（~300MB）。现已修正。
# ============================================================================

# --- 静态层（代码无关）---

# 安装 git（用于 nuwaxcode 源码安装）
RUN apt-get update && apt-get install -y \
    git \
    && rm -rf /var/lib/apt/lists/*

# 复制 .npmrc 文件
COPY docker/rcoder-master/.npmrc /root/.npmrc

# 安装 Rust（运行阶段也需 cargo：dev 容器内 cargo run/build）
# 用 rsproxy.cn（实测 ≈26 MB/s，远快于 USTC ≈0.1 MB/s）
ENV RUSTUP_DIST_SERVER=https://rsproxy.cn
ENV RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && \
    echo 'export PATH="/root/.cargo/bin:$PATH"' >> /root/.bashrc && \
    export PATH="/root/.cargo/bin:$PATH" && \
    which cargo && cargo --version
ENV PATH="/root/.cargo/bin:${PATH}"

# 复制 cargo 镜像配置文件（使用国内镜像加速）
COPY .cargo/config.toml /root/.cargo/config.toml

# npm 工具安装（CACHEBUST 刻意 bust 拿最新版；CACHEBUST 不变则缓存）
ARG CACHEBUST=1
RUN  echo "Cache bust: ${CACHEBUST}" && npm install -g claude-code-acp-ts@latest

# 安装 codex-acp（Codex ACP 代理）
RUN echo "Cache bust: ${CACHEBUST}" && npm install -g @zed-industries/codex-acp

# 安装 nuwaxcode（频繁更新的工具，放在最终镜像以加快构建）
RUN echo "Cache bust: ${CACHEBUST}" && npm i -g nuwaxcode@latest

# 安装 nuwax-file-server（文件服务器）
RUN echo "Cache bust: ${CACHEBUST}" && npm i -g nuwax-file-server@latest

# 根据codex-acp改造的codex cli,使用cli是: nuwax-codex-acp
RUN  echo "Cache bust: ${CACHEBUST}" &&  npm install -g nuwax-codex-acp@latest

# 配置 nuwaxcode - 禁用 websearch, webfetch 和 question 权限
RUN mkdir -p /root/.config/opencode && \
    echo '{\n  "permission": {\n    "websearch": "deny",\n    "webfetch": "deny",\n    "question": "deny"\n  }\n}' > /root/.config/opencode/opencode.json

# ============================================================================
# 🖥️ ttyd Web 终端配置
# ============================================================================
# 复制 ttyd 启动脚本和自定义前端页面
COPY docker/rcoder-master/ttyd/start-ttyd.sh /usr/local/bin/start-ttyd.sh
COPY docker/rcoder-master/ttyd/ttyd-index.html /usr/local/share/ttyd/index.html
RUN chmod +x /usr/local/bin/start-ttyd.sh

# 复制 agent-runner 启动脚本（同时启动 ttyd 和 agent_runner）
COPY docker/rcoder-master/agent-runner-start.sh /app/agent-runner-start.sh
RUN chmod +x /app/agent-runner-start.sh

# --- 代码相关层（放最后！代码改动只让这之后重跑） ---

# 创建 bin 目录并复制构建产物（来自 builder 阶段，代码改动即变）
RUN mkdir -p /app/bin
COPY --from=rust-builder /build/target/release/rcoder /app/bin/rcoder
COPY --from=rust-builder /build/target/release/agent_runner /app/bin/agent_runner

# 只有在二进制文件存在时才复制（避免feature构建时的错误）
RUN if [ -f /build/target/release/codex-acp-agent ]; then \
    cp /build/target/release/codex-acp-agent /app/bin/codex-acp-agent; \
    fi

# 设置执行权限
RUN chmod +x /app/bin/rcoder /app/bin/agent_runner

# 暴露端口
EXPOSE 8086
EXPOSE 60000
EXPOSE 80
EXPOSE 7681

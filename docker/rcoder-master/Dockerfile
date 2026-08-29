# ============================================================================
# 最终镜像 Dockerfile - master-rcoder
# ============================================================================
# 此 Dockerfile 使用多阶段构建：
# 1. 第一阶段：在 rust:bookworm 中构建 Rust 项目
# 2. 第二阶段：基于 master-rcoder-base 基础镜像运行
# ============================================================================

# 全局 ARG 声明（必须在第一个 FROM 之前，才能在后续 FROM 中使用）
ARG BASE_IMAGE=dev-master-rcoder-base:latest

# ============================================================================
# 第一阶段：构建 Rust 项目
# ============================================================================
FROM rust:bookworm AS rust-builder

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

# 安装 Rust 构建所需的额外系统依赖
# rust:bookworm (= buildpack-deps:bookworm 完整版) 已预装 build-essential / libssl-dev / git / curl 等,
# 这里补镜像里没有的: cmake + protobuf (rcoder/agent_runner/file-server gRPC 依赖); pkg-config 保留兜底
RUN apt-get update && apt-get install -y \
    cmake \
    pkg-config \
    protobuf-compiler \
    libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/*

# Rust 工具链已由 rust:bookworm 镜像提供 (cargo 在 /usr/local/cargo/bin, CARGO_HOME=/usr/local/cargo, 已在 PATH)。
# builder 阶段 cargo 镜像 (rsproxy-sparse) 写到 /usr/local/cargo/config.toml 生效
# (仓库根 .cargo/config.toml 为空, 故用 printf 内联生成, 不污染宿主)
RUN mkdir -p /usr/local/cargo && \
    printf '[source.crates-io]\nreplace-with = "rsproxy-sparse"\n[source.rsproxy-sparse]\nregistry = "sparse+https://rsproxy.cn/index/"\n' \
    > /usr/local/cargo/config.toml

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

# Rust 工具链 + cargo rsproxy 配置 已由 base (rust:bookworm) 提供:
# cargo 在 /usr/local/cargo/bin, config 在 /usr/local/cargo/config.toml (base COPY)。
# dev 容器 cargo run/build 直接可用, 不再单独 rustup 安装。

# npm 工具安装（CACHEBUST 刻意 bust 拿最新版；CACHEBUST 不变则缓存）
ARG CACHEBUST=1
RUN  echo "Cache bust: ${CACHEBUST}" && npm install -g claude-code-acp-ts@latest

# 安装 codex-acp（Codex ACP 代理）
RUN echo "Cache bust: ${CACHEBUST}" && npm install -g @zed-industries/codex-acp

# 安装 nuwaxcode（频繁更新的工具，放在最终镜像以加快构建）
# 使用官方 npm 源，国内镜像可能未同步平台二进制包（nuwaxcode-linux-arm64 等）
RUN echo "Cache bust: ${CACHEBUST}" && npm i -g nuwaxcode@latest --registry=https://registry.npmjs.org/

# 安装 nuwax-file-server（文件服务器）
# 使用官方 npm 源，国内镜像可能未同步
RUN echo "Cache bust: ${CACHEBUST}" && npm i -g nuwax-file-server@latest --registry=https://registry.npmjs.org/

# 根据codex-acp改造的codex cli,使用cli是: nuwax-codex-acp
# 使用官方 npm 源，国内镜像可能未同步
RUN  echo "Cache bust: ${CACHEBUST}" &&  npm install -g nuwax-codex-acp@latest --registry=https://registry.npmjs.org/

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

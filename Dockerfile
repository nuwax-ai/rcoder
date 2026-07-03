# DevSpace 开发专用 Dockerfile
# 基于 Rust 官方镜像，支持增量编译

FROM rust:1.95

# 设置环境变量
ENV TZ=Asia/Shanghai
ENV CARGO_HOME=/usr/local/cargo
ENV PATH="/usr/local/cargo/bin:${PATH}"

# 使用 LinuxMirrors 一键配置阿里云镜像源
RUN curl -sSL https://linuxmirrors.cn/main.sh | bash -s -- \
    --source mirrors.aliyun.com \
    --protocol https \
    --use-intranet-source false \
    --install-epel false \
    --backup false \
    --upgrade-software false \
    --clean-cache true \
    --ignore-backup-tips

# 安装必要的运行时依赖
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
    # 编译工具
    build-essential \
    cmake \
    pkg-config \
    # protobuf 编译器（gRPC 项目必需）
    protobuf-compiler \
    libprotobuf-dev \
    # 其他工具
    vim \
    htop \
    && rm -rf /var/lib/apt/lists/* \
    && ln -snf /usr/share/zoneinfo/$TZ /etc/localtime \
    && echo $TZ > /etc/timezone

# ============================================================================
# ttyd Web 终端（本地开发测试用）
# - agent_runner 的 ws_terminal 中间层会 connect 本地 ttyd（127.0.0.1:7681）做 cd 控制
# - 与正式镜像（build_config/rcoder）保持一致，方便本地复现 web ttyd 场景
# 下载走 gh-proxy 镜像（国内开发友好）
# ============================================================================
ARG TTYD_VERSION=1.7.7
RUN _ARCH="$(dpkg --print-architecture)" && \
    case "${_ARCH}" in \
        "amd64") TTYD_BIN="ttyd.x86_64" ;; \
        "arm64") TTYD_BIN="ttyd.aarch64" ;; \
        *) echo "Unsupported architecture: ${_ARCH}" && exit 1 ;; \
    esac && \
    DOWNLOAD_URL="https://gh-proxy.org/https://github.com/tsl0922/ttyd/releases/download/${TTYD_VERSION}/${TTYD_BIN}" && \
    curl -fsSL -o /usr/local/bin/ttyd "${DOWNLOAD_URL}" && \
    chmod +x /usr/local/bin/ttyd && \
    mkdir -p /usr/local/share/ttyd && \
    ttyd --version

# ttyd 启动脚本 + 自定义前端页面（复用 rcoder-master 的，保持与正式镜像一致）
COPY docker/rcoder-master/ttyd/start-ttyd.sh /usr/local/bin/start-ttyd.sh
COPY docker/rcoder-master/ttyd/ttyd-index.html /usr/local/share/ttyd/index.html
RUN chmod +x /usr/local/bin/start-ttyd.sh

ENV TTYD_PORT=7681

# 配置 Cargo 使用国内镜像源
RUN mkdir -p /usr/local/cargo && \
    echo '[source.crates-io]' > /usr/local/cargo/config.toml && \
    echo 'replace-with = "ustc"' >> /usr/local/cargo/config.toml && \
    echo '' >> /usr/local/cargo/config.toml && \
    echo '[source.ustc]' >> /usr/local/cargo/config.toml && \
    echo 'registry = "sparse+https://mirrors.ustc.edu.cn/crates.io-index/"' >> /usr/local/cargo/config.toml

# 设置工作目录
WORKDIR /app

# 暴露端口
EXPOSE 8090 8088 7681

# 默认命令（devspace 会覆盖）
CMD ["sleep", "infinity"]

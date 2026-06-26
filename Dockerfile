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

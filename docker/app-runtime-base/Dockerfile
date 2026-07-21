# 应用运行时基础镜像
# 包含常用排查工具、ttyd、基础服务支持

ARG DOCKER_MIRROR
FROM ${DOCKER_MIRROR}ubuntu:24.04

# 设置环境变量
ENV DEBIAN_FRONTEND=noninteractive
ENV LANG=C.UTF-8
ENV TTYD_VERSION=1.7.7

# 设置工作目录
WORKDIR /app

# 安装基础依赖和常用排查工具
RUN apt-get update && apt-get install -y \
    curl \
    wget \
    gnupg2 \
    lsb-release \
    ca-certificates \
    software-properties-common \
    supervisor \
    # 网络排查工具
    dnsutils \
    net-tools \
    iputils-ping \
    telnet \
    tcpdump \
    traceroute \
    # 系统排查工具
    procps \
    htop \
    lsof \
    strace \
    sysstat \
    iotop \
    # 文件工具
    vim \
    nano \
    tree \
    jq \
    less \
    # 其他工具
    zip \
    unzip \
    tar \
    gzip \
    rsync \
    git \
    && rm -rf /var/lib/apt/lists/*

# 安装 ttyd (Web 终端)
RUN ARCH=$(dpkg --print-architecture) && \
    case "${ARCH}" in \
        "amd64") TTYD_BIN="ttyd.x86_64" ;; \
        "arm64") TTYD_BIN="ttyd.aarch64" ;; \
        *) echo "Unsupported arch: ${ARCH}" && exit 1 ;; \
    esac && \
    curl -fsSL -o /usr/local/bin/ttyd "https://github.com/tsl0922/ttyd/releases/download/${TTYD_VERSION}/${TTYD_BIN}" && \
    chmod +x /usr/local/bin/ttyd && \
    mkdir -p /usr/local/share/ttyd

# 复制 ttyd 启动脚本
COPY start-ttyd.sh /usr/local/bin/start-ttyd.sh
RUN chmod +x /usr/local/bin/start-ttyd.sh

# 创建应用目录结构
RUN mkdir -p /app/{code,data,config,logs}

# 暴露端口
# 8080 - 应用端口
# 7681 - ttyd Web 终端
EXPOSE 8080 7681

# 健康检查
HEALTHCHECK --interval=30s --timeout=10s --start-period=60s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1

# 启动命令
CMD ["/usr/local/bin/start-ttyd.sh"]

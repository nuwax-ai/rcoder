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

# 安装 PostgreSQL 16
RUN install -d /usr/share/postgresql-keyring && \
    curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc \
        | gpg --dearmor -o /usr/share/postgresql-keyring/pgdg.gpg && \
    chmod 644 /usr/share/postgresql-keyring/pgdg.gpg && \
    echo "deb [signed-by=/usr/share/postgresql-keyring/pgdg.gpg] http://apt.postgresql.org/pub/repos/apt noble-pgdg main" \
        > /etc/apt/sources.list.d/pgdg.list && \
    apt-get update && apt-get install -y postgresql-16 && rm -rf /var/lib/apt/lists/*

# 安装 pgweb(Go 单二进制,PostgreSQL Web UI)
ARG PGWEB_VERSION=0.17.0
RUN ARCH=$(dpkg --print-architecture) && \
    case "${ARCH}" in \
        amd64) PGWEB_ARCH=amd64;; \
        arm64) PGWEB_ARCH=arm64;; \
        *) echo "Unsupported arch: ${ARCH}" && exit 1;; \
    esac && \
    curl -fsSL -o /tmp/pgweb.zip \
        "https://github.com/sosedoff/pgweb/releases/download/v${PGWEB_VERSION}/pgweb_linux_${PGWEB_ARCH}.zip" && \
    cd /tmp && unzip -o pgweb.zip && \
    mv "pgweb_linux_${PGWEB_ARCH}" /usr/local/bin/pgweb && \
    chmod +x /usr/local/bin/pgweb && \
    rm -f /tmp/pgweb.zip

# 复制 supervisor 配置 + ENTRYPOINT 启动脚本
COPY app-runtime.supervisor.conf /etc/supervisor/conf.d/app-runtime.conf
COPY start-app.sh /usr/local/bin/start-app.sh
RUN chmod +x /usr/local/bin/start-app.sh

# PG 首次 initdb wrapper(异步, 由 supervisor [program:postgresql] 托管)
COPY pg-supervisor-entry.sh /usr/local/bin/pg-supervisor-entry.sh
RUN chmod +x /usr/local/bin/pg-supervisor-entry.sh

# 创建应用目录结构
RUN mkdir -p /app/{code,data,config,logs}

# PG / pgweb 默认配置(可被 UserApp env 覆盖)
# POSTGRES_PASSWORD 不设默认(敏感),由 UserApp env 提供;start-app.sh 兜底 "app"
ENV PGDATA=/app/data/pg \
    POSTGRES_USER=app \
    POSTGRES_DB=app \
    PGWEB_PORT=8081

# 暴露端口:8080 应用 / 7681 ttyd / 8081 pgweb / 5432 PostgreSQL(仅容器内)
EXPOSE 8080 7681 8081 5432

# ENTRYPOINT:start-app.sh 起 PG+pgweb+ttyd(supervisor)+ 用户 command
# CMD 被 UserApp 的 command 覆盖(作为 args 传给 start-app.sh)
ENTRYPOINT ["/usr/local/bin/start-app.sh"]
CMD ["sleep", "infinity"]

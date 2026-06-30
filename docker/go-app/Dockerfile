# Go + PostgreSQL + Node + Nginx + etcd 应用容器
# 基于 Ubuntu 24.04 构建多服务容器

FROM ubuntu:24.04

# 设置环境变量
ENV DEBIAN_FRONTEND=noninteractive
ENV LANG=C.UTF-8
ENV GO_VERSION=1.22
ENV NODE_VERSION=24
ENV PG_VERSION=15
ENV ETCD_VERSION=v3.5.17
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
    build-essential \
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
    && rm -rf /var/lib/apt/lists/*

# 安装 etcd
COPY install-etcd.sh /tmp/install-etcd.sh
RUN chmod +x /tmp/install-etcd.sh && /tmp/install-etcd.sh ${ETCD_VERSION} && rm /tmp/install-etcd.sh

# 安装 Go
RUN wget -q https://go.dev/dl/go${GO_VERSION}.linux-amd64.tar.gz -O /tmp/go.tar.gz \
    && tar -C /usr/local -xzf /tmp/go.tar.gz \
    && rm /tmp/go.tar.gz
ENV PATH="/usr/local/go/bin:${PATH}"
ENV GOPATH="/root/go"
ENV PATH="${GOPATH}/bin:${PATH}"

# 安装 ttyd (Web 终端)
RUN ARCH=$(dpkg --print-architecture) && 
    case "${ARCH}" in 
        "amd64") TTYD_BIN="ttyd.x86_64" ;; 
        "arm64") TTYD_BIN="ttyd.aarch64" ;; 
        *) echo "Unsupported arch: ${ARCH}" && exit 1 ;; 
    esac && 
    curl -fsSL -o /usr/local/bin/ttyd "https://github.com/tsl0922/ttyd/releases/download/${TTYD_VERSION}/${TTYD_BIN}" && 
    chmod +x /usr/local/bin/ttyd && 
    mkdir -p /usr/local/share/ttyd
# 安装 Node.js 18
RUN curl -fsSL https://deb.nodesource.com/setup_${NODE_VERSION}.x | bash - \
    && apt-get install -y nodejs \
    && npm install -g pm2 \
    && rm -rf /var/lib/apt/lists/*

# 安装 PostgreSQL 15
RUN sh -c 'echo "deb http://apt.postgresql.org/pub/repos/apt $(lsb_release -cs)-pgdg main" > /etc/apt/sources.list.d/pgdg.list' \
    && wget --quiet -O - https://www.postgresql.org/media/keys/ACCC4CF8.asc | apt-key add - \
    && apt-get update \
    && apt-get install -y postgresql-${PG_VERSION} postgresql-client-${PG_VERSION} \
    && rm -rf /var/lib/apt/lists/*

# 安装 Nginx
RUN apt-get update && apt-get install -y nginx \
    && rm -rf /var/lib/apt/lists/*

# 配置 PostgreSQL
RUN mkdir -p /var/run/postgresql && \
    chown -R postgres:postgres /var/run/postgresql && \
    chmod 2775 /var/run/postgresql

# 创建应用目录结构
RUN mkdir -p /app/{src,config,logs,data,bin} \
    && mkdir -p /app/data/{postgres,nginx,etcd} \
    && mkdir -p /app/src/{node,static} \
    && mkdir -p /var/log/supervisor

# 复制配置文件
COPY nginx.conf /etc/nginx/nginx.conf
COPY supervisord.conf /etc/supervisor/conf.d/supervisord.conf
COPY etcd.conf.yml /app/config/etcd.conf.yml
COPY start.sh /app/start.sh
COPY start-ttyd.sh /usr/local/bin/start-ttyd.sh
COPY init-db.sh /app/init-db.sh
COPY go.mod /app/go.mod

# 设置文件权限
RUN chmod +x /app/start.sh /app/init-db.sh

# 设置 PostgreSQL 数据目录
ENV PGDATA=/app/data/postgres

# 初始化 PostgreSQL
RUN su - postgres -c "/usr/lib/postgresql/${PG_VERSION}/bin/initdb -D /app/data/postgresql"

# 配置 PostgreSQL 允许本地连接
RUN echo "host all all 0.0.0.0/0 md5" >> /app/data/postgresql/pg_hba.conf \
    && echo "listen_addresses = '*'" >> /app/data/postgresql/postgresql.conf

# 暴露端口
# 80 - Nginx HTTP
# 443 - Nginx HTTPS
# 8080 - Go 应用
# 3000 - Node.js 应用
# 5432 - PostgreSQL
# 2379 - etcd 客户端端口
# 2380 - etcd 对等端口
EXPOSE 80 443 8080 3000 5432 2379 2380 7681

# 健康检查
HEALTHCHECK --interval=30s --timeout=10s --start-period=60s --retries=3 \
    CMD curl -f http://localhost/health || exit 1

# 启动命令
CMD ["/app/start.sh"]

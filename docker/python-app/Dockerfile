# Python + PostgreSQL + Node + Nginx + etcd 应用容器
# 基于 Ubuntu 24.04 构建多服务容器

FROM ubuntu:24.04

# 设置环境变量
ENV DEBIAN_FRONTEND=noninteractive
ENV LANG=C.UTF-8
ENV PYTHON_VERSION=3.11
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

# 安装 Python 3.11
RUN add-apt-repository ppa:deadsnakes/ppa \
    && apt-get update \
    && apt-get install -y \
    python${PYTHON_VERSION} \
    python${PYTHON_VERSION}-venv \
    python${PYTHON_VERSION}-dev \
    python3-pip \
    && rm -rf /var/lib/apt/lists/*

# 设置 Python 默认版本
RUN update-alternatives --install /usr/bin/python3 python3 /usr/bin/python${PYTHON_VERSION} 1 \
    && update-alternatives --install /usr/bin/python python /usr/bin/python${PYTHON_VERSION} 1

# 安装 Python 常用工具
RUN pip3 install --no-cache-dir --break-system-packages \
    pip \
    setuptools \
    wheel \
    virtualenv

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
RUN mkdir -p /app/{src,config,logs,data,venv} \
    && mkdir -p /app/data/{postgres,nginx,etcd} \
    && mkdir -p /var/log/supervisor

# 创建 Python 虚拟环境
RUN python${PYTHON_VERSION} -m venv /app/venv \
    && /app/venv/bin/pip install --no-cache-dir pip setuptools wheel

# 复制配置文件
COPY nginx.conf /etc/nginx/nginx.conf
COPY supervisord.conf /etc/supervisor/conf.d/supervisord.conf
COPY etcd.conf.yml /app/config/etcd.conf.yml
COPY start.sh /app/start.sh
COPY init-db.sh /app/init-db.sh
COPY requirements.txt /app/requirements.txt

# 设置文件权限
RUN chmod +x /app/start.sh /app/init-db.sh

# 安装 Python 依赖
RUN if [ -f /app/requirements.txt ]; then \
    /app/venv/bin/pip install --no-cache-dir -r /app/requirements.txt; \
    fi

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
# 8080 - Python 应用
# 3000 - Node.js 应用
# 5432 - PostgreSQL
# 2379 - etcd 客户端端口
# 2380 - etcd 对等端口
EXPOSE 80 443 8080 3000 5432 2379 2380

# 健康检查
HEALTHCHECK --interval=30s --timeout=10s --start-period=60s --retries=3 \
    CMD curl -f http://localhost/health || exit 1

# 启动命令
CMD ["/app/start.sh"]

# 应用运行时基础镜像（app-runtime-base）
# 基础设施层：Rust 工具链（来自 rust:1.97）+ 排查工具 + ttyd + PostgreSQL + pgweb + supervisor。
# rust:1.97 基于 Debian bookworm（buildpack-deps），Rust/cargo 白赚，不再单独 rustup 装。
# 语言运行时见 Dockerfile.runtime（FROM 本镜像 + Node/Python/Java/Go）。

ARG DOCKER_MIRROR

# === app-cli builder（从 vendored rcoder 源编译 app-cli 编排器；app-cli 独立于 rcoder workspace）===
FROM ${DOCKER_MIRROR}rust:1.97 AS app-cli-builder
WORKDIR /build
RUN apt-get update && apt-get install -y --no-install-recommends cmake libclang-dev protobuf-compiler && rm -rf /var/lib/apt/lists/*
COPY code/rcoder/ .
RUN cd crates/app-cli && cargo build --release

# === 主 stage（app-runtime-base 基础设施层）===
ARG DOCKER_MIRROR
FROM ${DOCKER_MIRROR}rust:1.97

# 设置环境变量
ENV DEBIAN_FRONTEND=noninteractive
ENV LANG=C.UTF-8
ENV TTYD_VERSION=1.7.7

# 设置工作目录
WORKDIR /app


# 使用 LinuxMirrors 一键配置阿里云镜像源（rust:1.97 基于 Debian trixie，已预装 curl）
RUN curl -sSL https://linuxmirrors.cn/main.sh | bash -s -- \
    --source mirrors.aliyun.com \
    --protocol https \
    --use-intranet-source false \
    --install-epel false \
    --backup false \
    --upgrade-software false \
    --clean-cache true \
    --ignore-backup-tips

# 安装基础依赖和常用排查工具（去掉 software-properties-common —— 不再用 deadsnakes PPA/add-apt-repository）
RUN apt-get update && apt-get install -y \
    curl \
    wget \
    gnupg2 \
    lsb-release \
    ca-certificates \
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

# 安装 PostgreSQL 16（rust:1.97 基于 Debian trixie，PG apt 仓库用 trixie-pgdg）
RUN install -d /usr/share/postgresql-keyring && \
    curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc \
        | gpg --dearmor -o /usr/share/postgresql-keyring/pgdg.gpg && \
    chmod 644 /usr/share/postgresql-keyring/pgdg.gpg && \
    echo "deb [signed-by=/usr/share/postgresql-keyring/pgdg.gpg] http://apt.postgresql.org/pub/repos/apt trixie-pgdg main" \
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

# 安装 pingap (pingora 反向代理，全特性版；官方 install.sh 自动识别 amd64/arm64)
# 锁定版本：install.sh 默认拉 latest 会随上游漂移、构建不可复现，
# 用 sed 把 get_latest_release 替换为固定 tag（arch/libc/full 处理仍走脚本原逻辑）。
# 升级 pingap：改下面 ARG + 同步 crates/app-cli/Cargo.toml 的 pingap-config rev + docker-compose 元数据。
ARG PINGAP_VERSION=v0.13.8
RUN curl -sSL https://raw.githubusercontent.com/vicanso/pingap/main/install.sh \
    | sed "s|version=\"\$(get_latest_release)\"|version=\"${PINGAP_VERSION}\"|" \
    | PINGAP_FULL=1 sh

# 安装 app-cli（UserApp 容器编排器，替代 start.sh；由 app-cli-builder stage 编译）
COPY --from=app-cli-builder /build/crates/app-cli/target/release/app-cli /usr/local/bin/app-cli

# 复制 supervisor 配置 + ENTRYPOINT 启动脚本
COPY app-runtime.supervisor.conf /etc/supervisor/conf.d/app-runtime.conf
COPY start-app.sh /usr/local/bin/start-app.sh
RUN chmod +x /usr/local/bin/start-app.sh

# PG 首次 initdb wrapper(异步, 由 supervisor [program:postgresql] 托管)
COPY pg-supervisor-entry.sh /usr/local/bin/pg-supervisor-entry.sh
RUN chmod +x /usr/local/bin/pg-supervisor-entry.sh

# npm 默认国内镜像源（加速：① Dockerfile.runtime 的 npm install -g；② UserApp 运行时 npm install）
COPY .npmrc /root/.npmrc

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

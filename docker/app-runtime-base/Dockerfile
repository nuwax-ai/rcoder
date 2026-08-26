# 应用运行时基础镜像（app-runtime-base）
# 基础设施层：Rust 工具链（来自 rust:trixie）+ 排查工具 + ttyd + PostgreSQL + pgweb + supervisor。
# rust:trixie 基于 Debian trixie（buildpack-deps），Rust/cargo 白赚，不再单独 rustup 装；Rust 版本随 tag 自动跟进最新 stable。
# 语言运行时见 Dockerfile.runtime（FROM 本镜像 + Node/Python/Java/Go）。

ARG DOCKER_MIRROR

# === app-cli builder（从 vendored rcoder 源编译 app-cli 编排器；app-cli 独立于 rcoder workspace）===
FROM ${DOCKER_MIRROR}rust:trixie AS app-cli-builder
WORKDIR /build
# builder 阶段先换国内 apt 源(默认 deb.debian.org 从国内拉易 stall),与 stage-2 的 LinuxMirrors 换源对齐
RUN curl -sSL https://linuxmirrors.cn/main.sh | bash -s -- \
        --source mirrors.aliyun.com --protocol https --use-intranet-source false \
        --install-epel false --backup false --upgrade-software false --clean-cache true --ignore-backup-tips && \
    apt-get update && apt-get install -y --no-install-recommends cmake libclang-dev protobuf-compiler && rm -rf /var/lib/apt/lists/*
COPY --from=rcoder / .
# 删 rust-toolchain.toml:rcoder 锁 1.96.0,但 base image rust:trixie 自带更新版(向下兼容)。
# 不删会触发 rustup 下载 1.96.0 工具链(~200MB,走 static.rust-lang.org 官方,国内极慢)。
RUN rm -f rust-toolchain.toml
# 配置 cargo 源(国内 rsproxy 加速)。文件在 .cargo/config.toml,查阅/修改方便。
COPY .cargo/config.toml /usr/local/cargo/config.toml
RUN cd crates/app-cli && cargo build --release
# 容器内文件服务单件（RBD 卷形态·容器中心化，file-server-proxy 0.2.0 起）：file-server-proxy
# default feature 自带 file-server lib 进程内直连（60000 AllRust，无独立 8086 进程/端口）。
# -p 显式选 package：裸 --bin 的解析范围是 workspace default-members（不含
# file-server-proxy），会报 no bin target ... in default-run packages。
RUN cargo build --release -p file-server-proxy

# === 主 stage（app-runtime-base 基础设施层）===
ARG DOCKER_MIRROR
FROM ${DOCKER_MIRROR}rust:trixie

# 设置环境变量
ENV DEBIAN_FRONTEND=noninteractive
ENV LANG=C.UTF-8
ENV TTYD_VERSION=1.7.7
# pingap 版本唯一来源 = makefiles/16-app-runtime.mk 的 PINGAP_VERSION/COMMIT(经 --build-arg 注入)。
# 此处不写默认值,避免与 mk 两处版本号需同步;裸 docker build 不传 build-arg 会 fail-fast
# (tarball 路径含空版本,test -f 失败)。build-app-runtime-base-* 目标均已传该 build-arg。
# PINGAP_COMMIT 标识 app-cli 使用的 pingap-config 源码版本,官方 release 二进制内部 commit 可能不同。
ARG PINGAP_VERSION
ARG PINGAP_COMMIT
ENV RCODER_PINGAP_VERSION=${PINGAP_VERSION} \
    RCODER_PINGAP_COMMIT=${PINGAP_COMMIT}

# 设置工作目录
WORKDIR /app


# 使用 LinuxMirrors 一键配置阿里云镜像源（rust:trixie 基于 Debian trixie，已预装 curl）
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
    util-linux \
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

# 安装 ttyd (Web 终端) — 二进制由 make download-ttyd-amd64/arm64 预下载 (见 09), COPY 进容器
ARG TARGETARCH
COPY downloads/ttyd-${TARGETARCH} /usr/local/bin/ttyd
RUN chmod +x /usr/local/bin/ttyd && mkdir -p /usr/local/share/ttyd

# 复制 ttyd 启动脚本
COPY start-ttyd.sh /usr/local/bin/start-ttyd.sh
RUN chmod +x /usr/local/bin/start-ttyd.sh

# 安装 PostgreSQL 16（rust:trixie 基于 Debian trixie，PG apt 仓库用 trixie-pgdg）
RUN install -d /usr/share/postgresql-keyring && \
    curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc \
        | gpg --dearmor -o /usr/share/postgresql-keyring/pgdg.gpg && \
    chmod 644 /usr/share/postgresql-keyring/pgdg.gpg && \
    echo "deb [signed-by=/usr/share/postgresql-keyring/pgdg.gpg] http://apt.postgresql.org/pub/repos/apt trixie-pgdg main" \
        > /etc/apt/sources.list.d/pgdg.list && \
    apt-get update && apt-get install -y postgresql-16 && rm -rf /var/lib/apt/lists/*

# 安装 pgweb(Go 单二进制) — 由 make download-pgweb (09) 预下载分发到 downloads/, COPY 进容器
COPY downloads/pgweb-${TARGETARCH} /usr/local/bin/pgweb
RUN chmod +x /usr/local/bin/pgweb

# 安装 dbx-web(DBX 数据库 Web GUI,60+ 数据库;supervisor 托管 :4224,见 app-runtime.supervisor.conf [program:dbx])
# 由 make download-dbx-cache (16) 从官方镜像 docker.cnb.cool/dbxio.com/dbx 抽取分发到 downloads/
# (与 agent-runner 镜像同源分发)。dbx-core 的 font-kit 枚举系统字体 → 需 fontconfig/libfreetype6
# + 至少一套字体(官方镜像同款依赖,rust:trixie 不保证自带)。
RUN apt-get update && apt-get install -y --no-install-recommends \
        fontconfig \
        fonts-dejavu-core \
        libfreetype6 \
        && rm -rf /var/lib/apt/lists/*
COPY downloads/dbx-web-${TARGETARCH} /usr/local/bin/dbx-web
COPY downloads/dbx-static/ /usr/local/share/dbx/static
RUN chmod +x /usr/local/bin/dbx-web
# 冒烟自检:dbx-web 无 CLI 参数(--version 会直接起服务),起临时实例探 /api/auth/check 后退出,
# 抓架构/glibc/缺库错误 fail-fast。
RUN DBX_DATA_DIR=/tmp/dbx-selfcheck DBX_PORT=4199 /usr/local/bin/dbx-web & \
    PID=$!; \
    OK=0; \
    for i in $(seq 1 15); do \
        curl -fsS http://127.0.0.1:4199/api/auth/check 2>/dev/null | grep -q setup_required && OK=1 && break; \
        sleep 1; \
    done; \
    kill $PID 2>/dev/null; \
    [ "$OK" = "1" ] || { echo "dbx-web self-check failed"; exit 1; }; \
    rm -rf /tmp/dbx-selfcheck; \
    echo "dbx-web self-check passed"

# 安装 Makefile 预下载到本地 cache/ 的 Pingap full release。
# 版本化文件名避免升级后误用旧缓存；校验二进制可运行且版本匹配。
COPY cache/ /tmp/cache/
RUN case "$(dpkg --print-architecture)" in \
        amd64) PINGAP_ARCH=x86 ;; \
        arm64) PINGAP_ARCH=aarch64 ;; \
        *) echo "Unsupported arch: $(dpkg --print-architecture)" && exit 1 ;; \
    esac && \
    TARBALL="/tmp/cache/pingap-v${PINGAP_VERSION}-linux-gnu-${PINGAP_ARCH}-full.tar.gz" && \
    test -f "$TARBALL" && \
    tar -C /usr/local/bin -xzf "$TARBALL" && \
    mv "/usr/local/bin/pingap-linux-gnu-${PINGAP_ARCH}-full" /usr/local/bin/pingap && \
    chmod +x /usr/local/bin/pingap && \
    PINGAP_INFO="$(pingap --version)" && \
    echo "$PINGAP_INFO" && \
    echo "$PINGAP_INFO" | grep -F "pingap ${PINGAP_VERSION}" && \
    rm -rf /tmp/cache

# 安装 app-cli（UserApp 容器编排器，替代 start.sh；由 app-cli-builder stage 编译）
COPY --from=app-cli-builder /build/crates/app-cli/target/release/app-cli /usr/local/bin/app-cli

# 容器内文件服务（supervisor 托管，见 app-runtime.supervisor.conf）：rcoder 转发层
# (x-app-stage: prod) 的上游。单二进制=file-server-proxy（0.2.0 起 file-server 以
# lib 直连集成，无独立 8086 进程；USERAPP_SINGLE_APP_ID=APP_ID 单 app 模式由
# proxy program 的 env 传入内嵌 file-server——rcoder 创建容器时注入，本地直跑
# 无 APP_ID → 空值=多 app 开发语义，无害）。
COPY --from=app-cli-builder /build/target/release/file-server-proxy /usr/local/bin/file-server-proxy

# 复制 supervisor 配置 + ENTRYPOINT 启动脚本
# 主配置显式化：unix socket XML-RPC 控制通道（app-cli server 动态托管用户服务）
COPY supervisord.conf /etc/supervisor/supervisord.conf
COPY app-runtime.supervisor.conf /etc/supervisor/conf.d/app-runtime.conf
COPY start-app.sh /usr/local/bin/start-app.sh
RUN chmod +x /usr/local/bin/start-app.sh

# PG 首次 initdb wrapper(异步, 由 supervisor [program:postgresql] 托管)
COPY pg-supervisor-entry.sh /usr/local/bin/pg-supervisor-entry.sh
RUN chmod +x /usr/local/bin/pg-supervisor-entry.sh

# npm 默认国内镜像源（加速：① Dockerfile.runtime 的 npm install -g；② UserApp 运行时 npm install）
COPY .npmrc /root/.npmrc

# 创建目录结构（本地直跑兜底路径 /app/*；容器部署时挂载卷优先生效，
# /home/user/{data,logs} 由挂载卷提供，此处 mkdir 仅直跑场景兜底）
RUN mkdir -p /app/code /app/config /home/user/data /home/user/logs /run/app-cli/pingap

# PG / pgweb / dbx-web / workspace 默认配置(可被 UserApp env 覆盖——rcoder 创建
# 容器时注入 USERAPP_WORKSPACE_DIR=/home/user/{app_id} 等；此层默认=本地直跑语义)
# POSTGRES_PASSWORD 不设默认(敏感),由 UserApp env 提供;start-app.sh 兜底 "app"
# DBX_PASSWORD 同理不设默认:首访浏览器自设(存 DBX_DATA_DIR/dbx.db),需固定密码时注入 env。
# USERAPP_WORKSPACE_DIR 默认 /app 兼作 supervisor conf %(ENV_…)s 展开的必存锚。
ENV PGDATA=/home/user/data/pg \
    POSTGRES_USER=app \
    POSTGRES_DB=app \
    PGWEB_PORT=8081 \
    DBX_PORT=4224 \
    DBX_STATIC_DIR=/usr/local/share/dbx/static \
    DBX_DATA_DIR=/home/user/data/dbx \
    USERAPP_WORKSPACE_DIR=/app \
    APP_ID=

# 暴露端口:9080/3010 应用+pingap / 7681 ttyd / 8081 pgweb / 4224 dbx-web / 5432 PostgreSQL(仅容器内)
# / 8086 file-server / 60000 file-server-proxy（rcoder 转发层 prod 上游）
EXPOSE 9080 3010 7681 8081 4224 5432 60000

# ENTRYPOINT:start-app.sh 起 PG+pgweb+ttyd(supervisor)+ app-cli。
# CMD 仅保留为镜像元数据；Manifest v1 的用户服务命令来自 release.lock.toml。
ENTRYPOINT ["/usr/local/bin/start-app.sh"]
CMD ["sleep", "infinity"]

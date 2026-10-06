# 应用运行时基础镜像（app-runtime-base）
# 基础设施 + 语言运行时基座：Rust 工具链（rust:trixie）+ 排查工具 + ttyd + PostgreSQL + supervisor
#   + Node/Python/Java/Go/Deno 运行时本体（变化慢，月级 → 进 base）。
# rust:trixie 基于 Debian trixie（buildpack-deps），Rust/cargo 白赚，不再单独 rustup 装；Rust 版本随 tag 自动跟进最新 stable。
# ⚠️ 分层原则（与 build-agent-docker 65a1cb8/878c3f6 同步）——越稳定越靠下：npm 全局工具
# （几乎每次发版变）与 app-cli / file-server-proxy（rcoder 源码产物，随源频繁变化）在
# Dockerfile.runtime（FROM 本镜像）安装/编译：二者变更不引发 base 层 digest 变化
# （父链失效 → runtime 全量重建）。base 不含任何 rcoder 源码依赖。
# ⚠️ 镜像体积：Rust 工具链 + 四语言运行时，约 4-5GB —— 全语言自由的代价
# （部署拉取 app-runtime 的总量不变，仅层归属变化）。
# ⚠️ Python 版本契约：与 agent-runner builder 镜像统一 rust:trixie（原生 3.13
# 双侧同 ABI）——builder pip 装的 wheel 在本运行时原生可载（cp313 ↔ cp313）。

ARG DOCKER_MIRROR

# === 主 stage（app-runtime-base 基础设施 + 语言运行时层）===
FROM ${DOCKER_MIRROR}rust:trixie

# 设置环境变量
ENV DEBIAN_FRONTEND=noninteractive
ENV LANG=C.UTF-8
ARG TTYD_VERSION=1.7.7
ENV TTYD_VERSION=${TTYD_VERSION}
ARG NODE_RUNTIME_VERSION=22.23.2
ARG DENO_VERSION=2.9.7
# pingap 版本唯一来源 = docker/build-app-runtime.py 注入（与 make/docker.mk 的
# dev agent-runner 构建同值；单一事实源 = app-cli devtool.rs，由 pingap_version_gate.py 核对）。
# 此处不写默认值,避免多处版本号需同步;裸 docker build 不传 build-arg 会 fail-fast
# (tarball 路径含空版本,test -f 失败)。
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

# 安装 ttyd (Web 终端) — 二进制预下载到 downloads/（与 build-agent-docker 09 同源分发）, COPY 进容器
ARG TARGETARCH
COPY downloads/ttyd-${TARGETARCH} /usr/local/bin/ttyd
RUN chmod +x /usr/local/bin/ttyd && ttyd --version 2>&1 | grep -F "${TTYD_VERSION}" && mkdir -p /usr/local/share/ttyd

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
    # 钉死 postgres uid/gid=111:121（与 agent-runner 镜像舰队统一）：Debian 系统用户 uid
    # 动态分配随包集漂移（本镜像 trixie 现值实测 102:105，bookworm 时代值已不可考），
    # 持久卷 /home/user/data/pg（UserApp workspace PVC）旧数据属主对不上 → postgres
    # FATAL "Permission denied"。预创建后 postgresql-common 复用既有用户；历史遗留 uid
    # 由 start-app.sh 的属主检测 chown 一次性收敛。
    groupadd -g 121 postgres && \
    useradd -r -u 111 -g 121 -d /var/lib/postgresql -s /bin/bash postgres && \
    apt-get update && apt-get install -y postgresql-16 && rm -rf /var/lib/apt/lists/*

# （pgweb 已退役删除——数据库 Web 控制台由 dbx 承担）

# 安装 dbx-web(DBX 数据库 Web GUI,60+ 数据库;supervisor 托管 :4224,见 app-runtime.supervisor.conf [program:dbx])
# 预下载分发到 downloads/（与 build-agent-docker 16 同源）。dbx-core 的 font-kit 枚举系统字体 →
# 需 fontconfig/libfreetype6 + 至少一套字体(官方镜像同款依赖,rust:trixie 不保证自带)。
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

# 安装预下载到本地 cache/ 的组件（pingap / node / go tarball / deno 二进制）。
# 版本化文件名避免升级后误用旧缓存；校验二进制可运行且版本匹配。
# 注：/tmp/cache 的最终清理由 Go 段完成（node/deno 段各自只清自己的文件）。
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
    echo "$PINGAP_INFO" | grep -F "pingap ${PINGAP_VERSION}"

# ── Node.js 22（prebuilt tarball, 绕过 NodeSource 签名 key 不稳）───────────────
# node prebuilt (含 npm) 预下载到 cache/（与 build-agent-docker download-node 同源同版本）。
# node 本体变化慢 → 进 base；npm 全局工具(pm2/dbx-mcp 等, 高频变)留在 Dockerfile.runtime。
# 注: Node prebuilt 文件名用 x64 (非 amd64), dpkg 映射 amd64→x64。
RUN ARCH=$(dpkg --print-architecture) \
    && case "$ARCH" in amd64) NA=x64 ;; arm64) NA=arm64 ;; *) echo "❌ Unsupported arch: $ARCH" >&2; exit 1 ;; esac \
    && test -f /tmp/cache/node-v${NODE_RUNTIME_VERSION}-linux-${NA}.tar.gz || { echo "❌ Node tarball missing in cache/"; exit 1; } \
    && tar -C /usr/local -xzf /tmp/cache/node-v${NODE_RUNTIME_VERSION}-linux-${NA}.tar.gz --strip-components=1 \
    && test "$(node --version)" = "v${NODE_RUNTIME_VERSION}" && npm --version \
    && npm config set fetch-retries 5 \
    && rm -rf /tmp/cache/node-*

# Deno prebuilt (deno.land/install.sh 国内不稳, 改预下载). deno-<arch> 单二进制
# 预下载分发到 cache/, 上面的 COPY cache/ 带入; 此处 dpkg 选架构 install 到 /usr/local/bin
RUN ARCH=$(dpkg --print-architecture) \
    && [ -f "/tmp/cache/deno-${ARCH}" ] || { echo "❌ Deno missing: /tmp/cache/deno-${ARCH}"; exit 1; } \
    && install -m 0755 /tmp/cache/deno-${ARCH} /usr/local/bin/deno \
    && deno --version | grep -F "deno ${DENO_VERSION} "

# ── Python（rust:trixie 自带 3.13）+ uv ────────────────────────────────────────
# uv（Astral 极速 Python 包管理器，最新版）经 pip 装到 /usr/local/bin，在默认 PATH。
RUN apt-get update && apt-get install -y \
       python3 \
       python3-pip \
       python3-venv \
       python3-dev \
    && rm -rf /var/lib/apt/lists/* \
    && ln -sf /usr/bin/python3 /usr/bin/python \
    && pip3 install --no-cache-dir --break-system-packages --ignore-installed pip setuptools wheel virtualenv uv

# ── Java (openjdk-25 + Maven + Gradle + Arthas) ─────────────────────────────────
# trixie 官方仓库原生提供 openjdk-25（bookworm 时代缺位曾走 Adoptium Temurin 源；
# 与 agent-runner builder 的 JDK 段同源同版，builder 编 target=25 与运行时 25 对齐）。
# apt 网络硬化：镜像 CDN 大文件（openjdk/maven 传递依赖）在构建网络下偶发传输
# 挂起，apt 默认无整体超时会永久停住构建（2026-09-30 两轮实测同层卡死）。
# Timeout 覆盖连接与低速中断，Retries 使中断后重试自愈。
RUN apt-get -o Acquire::https::Timeout=90 -o Acquire::http::Timeout=90 -o Acquire::Retries=8 update && \
    apt-get -o Acquire::https::Timeout=90 -o Acquire::http::Timeout=90 -o Acquire::Retries=8 install -y maven gradle openjdk-25-jdk && \
    ARCH=$(dpkg --print-architecture) && \
    ln -sf /usr/lib/jvm/java-25-openjdk-${ARCH} /usr/lib/jvm/java-25-openjdk-current && \
    java -version && javac -version && \
    rm -rf /var/lib/apt/lists/* \
    && curl -fsSL https://arthas.aliyun.com/arthas-boot.jar -o /usr/local/bin/arthas-boot.jar \
    && printf '#!/bin/bash\njava -jar /usr/local/bin/arthas-boot.jar "$@"\n' > /usr/local/bin/arthas \
    && chmod +x /usr/local/bin/arthas
ENV JAVA_HOME=/usr/lib/jvm/java-25-openjdk-current
ENV PATH="${JAVA_HOME}/bin:${PATH}"

# ── Go ───────────────────────────────────────────────────────────────────────────
# Go tarball 预下载到 cache/（与 build-agent-docker download-go-cache 同源同版本）;
# 缺失直接报错 (fail fast)。本段末尾统一清理 /tmp/cache（node/deno 段的残留也在此清掉）。
ARG GO_VERSION=1.26.4
ENV GO_VERSION=${GO_VERSION}
RUN ARCH=$(dpkg --print-architecture) \
    && TARBALL="/tmp/cache/go${GO_VERSION}.linux-${ARCH}.tar.gz" \
    && [ -f "$TARBALL" ] || { echo "❌ Go tarball missing: $TARBALL"; exit 1; } \
    && echo "Using cached Go: $TARBALL" \
    && tar -C /usr/local -xzf "$TARBALL" \
    && /usr/local/go/bin/go version | grep -F " go${GO_VERSION} " \
    && rm -rf /tmp/cache
ENV GOPATH=/root/go
# 多源冗余：七牛 → 阿里 → goproxy.io → direct（与 rcoder-agent-runner/Dockerfile.base 对齐）
ENV GOPROXY=https://goproxy.cn,https://mirrors.aliyun.com/goproxy/,https://goproxy.io,direct
# 清理旧基础镜像可能遗留的下载代理改写。Git smart HTTP 必须直连；
# unset-all 的退出码 5 仅表示键不存在，其他配置错误仍使构建失败。
RUN git config --global --unset-all url."https://gh-proxy.org/https://github.com/".insteadOf || test "$?" -eq 5

# ── 国内 pip/uv 镜像源（腾讯）─────────────────────────────────────────────────────
# 本链此前 pip/uv 无任何镜像配置 = pypi.org 官方直连（npm/pnpm 已由 .npmrc
# 走 npmmirror，无需处理）。官方直连国内速度不稳；aliyun pypi CDN
# 又对 HTTP/1.1 限速 ~20-40kB/s（2026-09-29 实测同 URL H2=2.9MB/s vs 1.1=22kB/s），
# 统一走腾讯源（HTTP/1.1 实测 7MB/s）。（与 build-agent-docker f5d655a 同步）
# pip：用户级配置优先于全局，三处同写保持一致（对齐 rcoder-agent-runner 末层同款）；
#      UserApp 运行容器 PVC 只挂 /home/user 子目录，/home/user/.config 属镜像层可安全覆盖。
# uv：用户级 uv.toml（不用 ENV —— 项目 pyproject 的 [[index]] 仍可覆盖，保持项目级优先语义）。
# trixie 的 python3-pip 不再自带 /etc/pip 目录，须先建（否则 pip.conf 写入失败）。
RUN mkdir -p /etc/pip && \
    printf '[global]\nindex-url = https://mirrors.cloud.tencent.com/pypi/simple\ntrusted-host = mirrors.cloud.tencent.com\ntimeout = 120\n\n[install]\ntrusted-host = mirrors.cloud.tencent.com\n' > /etc/pip/pip.conf && \
    mkdir -p /root/.config/pip /home/user/.config/pip /root/.config/uv && \
    cp /etc/pip/pip.conf /root/.config/pip/pip.conf && \
    cp /etc/pip/pip.conf /home/user/.config/pip/pip.conf && \
    printf '[[index]]\nurl = "https://mirrors.cloud.tencent.com/pypi/simple"\ndefault = true\n' > /root/.config/uv/uv.toml

# 聚合 PATH：Go + Go bin（node 在 /usr/local/bin、python/java 在默认 PATH，无需追加）
ENV PATH="/usr/local/go/bin:${GOPATH}/bin:${PATH}"

# 复制 supervisor 配置 + ENTRYPOINT 启动脚本
# 主配置显式化：unix socket XML-RPC 控制通道（app-cli server 动态托管用户服务）
COPY supervisord.conf /etc/supervisor/supervisord.conf
COPY app-runtime.supervisor.conf /etc/supervisor/conf.d/app-runtime.conf
COPY start-app.sh /usr/local/bin/start-app.sh
RUN chmod +x /usr/local/bin/start-app.sh

# PG 首次 initdb wrapper(异步, 由 supervisor [program:postgresql] 托管)
COPY pg-admin-identity.sh /usr/local/bin/pg-admin-identity.sh
COPY pg-supervise.py /usr/local/bin/pg-supervise.py
COPY pg-supervisor-entry.sh /usr/local/bin/pg-supervisor-entry.sh
RUN chmod +x /usr/local/bin/pg-supervisor-entry.sh

# npm 默认国内镜像源（加速：① Dockerfile.runtime 的 npm install -g；② UserApp 运行时 npm install）
COPY .npmrc /root/.npmrc

# 创建目录结构（本地直跑兜底路径 /app/*；容器部署时挂载卷优先生效，
# /home/user/{data,logs} 由挂载卷提供，此处 mkdir 仅直跑场景兜底）
RUN mkdir -p /app/code /app/config /home/user/data /home/user/logs /run/app-cli/pingap

# PG / dbx-web / workspace 默认配置(可被 UserApp env 覆盖——rcoder 创建
# 容器时注入 USERAPP_WORKSPACE_DIR=/home/user/{app_id} 等；此层默认=本地直跑语义)
# PG 默认凭证 dev/dev/dev(与 agent-runner 对齐;仅首次 initdb 生效,存量 PGDATA 不受影响)
# DBX_PASSWORD 同理不设默认:首访浏览器自设(存 DBX_DATA_DIR/dbx.db),需固定密码时注入 env。
# USERAPP_WORKSPACE_DIR 默认 /app 兼作 supervisor conf %(ENV_…)s 展开的必存锚。
ENV PGDATA=/home/user/data/pg \
    POSTGRES_USER=dev \
    POSTGRES_PASSWORD=dev \
    POSTGRES_DB=dev \
    DBX_PORT=4224 \
    DBX_STATIC_DIR=/usr/local/share/dbx/static \
    DBX_DATA_DIR=/home/user/data/dbx \
    USERAPP_WORKSPACE_DIR=/app \
    APP_ID=

# 暴露端口:9080/3010 应用+pingap / 7681 ttyd / 4224 dbx-web / 5432 PostgreSQL(仅容器内)
# / 8086 file-server / 60000 file-server-proxy（rcoder 转发层 prod 上游）
EXPOSE 9080 3010 7681 4224 5432 60000

# ENTRYPOINT:start-app.sh 起 PG+dbx+ttyd(supervisor)+ app-cli。
# CMD 仅保留为镜像元数据；Manifest v1 的用户服务命令来自 release.lock.toml。
ENTRYPOINT ["/usr/local/bin/start-app.sh"]
CMD ["sleep", "infinity"]

# Uses the bundled BuildKit Dockerfile frontend; no extra frontend image pull.
# Runtime images contain browsers/language tools; their resolved digests are in the receipt.
ARG RUST_IMAGE=rust:1.95-trixie
ARG RCODER_BASE
ARG COMPUTER_BASE
ARG RUNTIME_BASE
FROM ${RUST_IMAGE} AS build
ARG APT_MIRROR=http://deb.debian.org
RUN sed -i "s#http://deb.debian.org#${APT_MIRROR}#g" /etc/apt/sources.list.d/debian.sources
RUN apt-get update && apt-get install -y --no-install-recommends cmake pkg-config protobuf-compiler libprotobuf-dev libclang-dev && rm -rf /var/lib/apt/lists/*
ARG CARGO_MIRROR=
RUN if [ -n "$CARGO_MIRROR" ]; then mkdir -p /usr/local/cargo && printf '[source.crates-io]\nreplace-with = "remote-mirror"\n[source.remote-mirror]\nregistry = "%s"\n' "$CARGO_MIRROR" > /usr/local/cargo/config.toml; fi
WORKDIR /src
COPY . .
ARG CARGO_JOBS=4
ENV CARGO_TARGET_DIR=/cache/target CARGO_PROFILE_DEV_DEBUG=0 CARGO_NET_GIT_FETCH_WITH_CLI=true
RUN --mount=type=cache,id=rcoder-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=rcoder-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=rcoder-cargo-target,target=/cache/target,sharing=locked \
    # buildkit COPY 将全部源文件 mtime 归一化为同一值——cache mount 里上次的
    # cargo fingerprint 记录的 mtime 与之相等 → 增量判定 stale 失效（实测：
    # tasks.rs 变更后 cargo 未重编成员、cp 出旧 bin）。touch workspace 成员源
    # 强制重编（第三方依赖走缓存不受影响，成员重编 ~分钟级）。
    find /src/crates -name '*.rs' -exec touch {} + && \
    cargo build --locked --jobs ${CARGO_JOBS} --profile dev -p rcoder -p agent_runner -p file-server-proxy --features rcoder/rcoder-pg --bin rcoder --bin agent_runner --bin file-server-proxy && \
    cargo build --locked --jobs ${CARGO_JOBS} --profile dev --manifest-path crates/app-cli/Cargo.toml && \
    mkdir /out && cp /cache/target/debug/rcoder /cache/target/debug/agent_runner /cache/target/debug/app-cli /cache/target/debug/file-server-proxy /out/
FROM ${RCODER_BASE} AS rcoder
ENV TTYD_USER=root
COPY --link --from=build /out/rcoder /out/agent_runner /app/bin/
COPY --link --from=build /out/agent_runner /usr/local/bin/agent_runner
COPY --link docker/rcoder-master/agent-runner-start.sh /app/agent-runner-start.sh
# ttyd 行为跟随源码快照，不能继承旧基础镜像中的普通用户默认值。
COPY --link docker/rcoder-master/ttyd/start-ttyd.sh /usr/local/bin/start-ttyd.sh
RUN chmod 0755 /app/agent-runner-start.sh /usr/local/bin/start-ttyd.sh
FROM ${COMPUTER_BASE} AS computer
ENV TTYD_USER=root
# Credential identity and PG shutdown helpers belong to this source snapshot,
# not the potentially older published base image.
COPY --link --from=build /out/agent_runner /out/app-cli /usr/local/bin/
COPY --link docker/rcoder-agent-runner/ttyd/start-ttyd.sh /usr/local/bin/start-ttyd.sh
COPY --link docker/rcoder-agent-runner/supervisor/conf.d/ttyd.conf /etc/supervisor/conf.d/ttyd.conf
COPY --link docker/rcoder-agent-runner/pg-admin-identity.sh docker/rcoder-agent-runner/pg-supervise.py docker/rcoder-agent-runner/pg-supervisor-entry.sh /usr/local/bin/
# Snapshot transport can deliver these as 0600 (see the runtime stage note);
# postgres runs the entry as a non-root user and needs world-read. 2026-09-27
# personal-K8s finding: the missing bits made every fresh builder pod's
# postgresql FATAL at pg-supervisor-entry.sh:22.
RUN chmod 0755 /usr/local/bin/pg-supervisor-entry.sh /usr/local/bin/pg-admin-identity.sh /usr/local/bin/pg-supervise.py /usr/local/bin/start-ttyd.sh && \
    chmod 0644 /etc/supervisor/conf.d/ttyd.conf
FROM ${RUNTIME_BASE} AS runtime
COPY --link --from=build /out/app-cli /out/file-server-proxy /usr/local/bin/
COPY --link docker/app-runtime-base/start-app.sh /usr/local/bin/start-app.sh
COPY --link docker/app-runtime-base/pg-admin-identity.sh docker/app-runtime-base/pg-supervise.py docker/app-runtime-base/pg-supervisor-entry.sh /usr/local/bin/
# COPY 保留同步源权限，而 Mutagen 同步产物是 0700（组/其他无读位）：
# `chmod +x` 只补执行位会得到 0711，supervisord 以 postgres 用户跑
# /bin/sh 入口脚本时读不了文件（Permission denied → 固定 PG 永远起不来，
# 2026-09-27 个人 K8s userapp 三连败根因）。显式 0755 并覆盖全部三脚本。
RUN chmod 0755 /usr/local/bin/start-app.sh /usr/local/bin/pg-supervisor-entry.sh /usr/local/bin/pg-admin-identity.sh /usr/local/bin/pg-supervise.py

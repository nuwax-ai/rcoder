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
COPY --link --from=build /out/rcoder /out/agent_runner /app/bin/
COPY --link --from=build /out/agent_runner /usr/local/bin/agent_runner
COPY --link docker/rcoder-master/agent-runner-start.sh /app/agent-runner-start.sh
RUN chmod +x /app/agent-runner-start.sh
FROM ${COMPUTER_BASE} AS computer
# Credential identity and PG shutdown helpers belong to this source snapshot,
# not the potentially older published base image.
COPY --link --from=build /out/agent_runner /out/app-cli /usr/local/bin/
COPY --link docker/rcoder-agent-runner/pg-admin-identity.sh docker/rcoder-agent-runner/pg-supervise.py docker/rcoder-agent-runner/pg-supervisor-entry.sh /usr/local/bin/
RUN chmod +x /usr/local/bin/pg-supervisor-entry.sh
FROM ${RUNTIME_BASE} AS runtime
COPY --link --from=build /out/app-cli /out/file-server-proxy /usr/local/bin/
COPY --link docker/app-runtime-base/start-app.sh /usr/local/bin/start-app.sh
COPY --link docker/app-runtime-base/pg-admin-identity.sh docker/app-runtime-base/pg-supervise.py docker/app-runtime-base/pg-supervisor-entry.sh /usr/local/bin/
# git 模式为 100644，COPY 保留源权限——不 chmod 则容器启动即
# "exec: /usr/local/bin/start-app.sh: permission denied"（CrashLoop）。
# 对齐 rcoder-master / app-runtime-base Dockerfile 的既有惯例。
RUN chmod +x /usr/local/bin/start-app.sh /usr/local/bin/pg-supervisor-entry.sh

#!/bin/sh
# =============================================================================
# supervisor `program:postgresql` 入口（由 supervisor 以 user=postgres 运行）。
# UserApp(app-runtime)版: PG 首次 initdb 异步化 —— 不再在 start-app.sh 同步阻塞,
# 避免 UserApp 首启慢被 liveness 杀(restartPolicy=Always → CrashLoopBackOff)。
# PGDATA=/app/data/pg(PVC), 重启不丢; PG_VERSION 存在则跳过 initdb。
# 幂等 + 末尾 exec postgres 前台(supervisor 直追 PID, 无需 gosu/su)。
# 对齐 agent-runner 的 pg-supervisor-entry.sh, 仅默认值不同(PGDATA/user/db)。
# =============================================================================
set -u

PG_BIN=/usr/lib/postgresql/16/bin
: "${PGDATA:=/app/data/pg}"
: "${POSTGRES_USER:=app}"
: "${POSTGRES_PASSWORD:=app}"
: "${POSTGRES_DB:=app}"

if [ ! -s "$PGDATA/PG_VERSION" ]; then
    echo "[pg] first-time initdb at $PGDATA"
    # 清理上次被中断的残留 (PG_VERSION 缺失但目录非空 = 半成品 initdb), 保证干净重做
    if [ -d "$PGDATA" ] && [ -n "$(ls -A "$PGDATA" 2>/dev/null)" ]; then
        echo "[pg] cleaning leftover partial PGDATA"
        rm -rf "${PGDATA:?}/"* 2>/dev/null || true
    fi

    PWFILE="$(mktemp)"
    printf '%s\n' "$POSTGRES_PASSWORD" > "$PWFILE"
    chmod 600 "$PWFILE"
    # initdb 失败 → 退出非零 → supervisor autorestart 重试 (上面清理保证幂等)
    if ! "$PG_BIN/initdb" -D "$PGDATA" \
            --username="$POSTGRES_USER" --pwfile="$PWFILE" \
            --auth-host=scram-sha-256 --auth-local=trust; then
        rm -f "$PWFILE"
        echo "[pg] initdb failed, will retry on supervisor autorestart" >&2
        exit 1
    fi
    rm -f "$PWFILE"

    # 临时起停, 仅 unix socket 建业务库 (不占 TCP, 不影响后续 exec 的 postgres)
    "$PG_BIN/pg_ctl" -D "$PGDATA" \
        -o '-c listen_addresses= -c unix_socket_directories=/tmp' -w start || true
    "$PG_BIN/createdb" -h /tmp -U "$POSTGRES_USER" "$POSTGRES_DB" 2>/dev/null || true
    "$PG_BIN/pg_ctl" -D "$PGDATA" -m fast -w stop || true
    echo "[pg] initdb done (user=$POSTGRES_USER db=$POSTGRES_DB)"
fi

# 前台运行 postgres, 供 supervisor 直接托管 (PID 即 postgres 本体)
# chmod 700: postgres 要求 PGDATA u=rwx(0700)或 u=rwx,g=rx(0750); start-app.sh 的 install -d 默认建 755
# → postgres FATAL "data directory has invalid permissions" → pgweb 登录连不上 PG. 幂等兜底 (防 fsGroup/PVC 改权限).
chmod 700 "$PGDATA" 2>/dev/null || true
exec "$PG_BIN/postgres" -D "$PGDATA"

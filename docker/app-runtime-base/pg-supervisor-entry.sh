#!/bin/sh
# =============================================================================
# supervisor `program:postgresql` 入口（由 supervisor 以 user=postgres 运行）。
# UserApp(app-runtime)版: PG 首次 initdb 异步化 —— 不再在 start-app.sh 同步阻塞,
# 避免 UserApp 首启慢被 liveness 杀(restartPolicy=Always → CrashLoopBackOff)。
# PGDATA=/home/user/data/pg(挂载卷), 重启不丢; PG_VERSION 存在则跳过 initdb。
# 幂等；末尾交由进程所有者统一托管 postgres 和数据库初始化任务。
# 对齐 agent-runner 的 pg-supervisor-entry.sh, 仅默认值不同(PGDATA/user/db)。
# =============================================================================
set -u

PG_BIN=/usr/lib/postgresql/16/bin
: "${PGDATA:=/home/user/data/pg}"
: "${POSTGRES_USER:=dev}"
: "${POSTGRES_PASSWORD:=dev}"
: "${POSTGRES_DB:=dev}"

. /usr/local/bin/pg-admin-identity.sh
pg_admin_identity_load || exit 1

if [ ! -s "$PGDATA/PG_VERSION" ]; then
    echo "[pg] first-time initdb at $PGDATA"
    # 清理上次被中断的残留 (PG_VERSION 缺失但目录非空 = 半成品 initdb), 保证干净重做
    if [ -d "$PGDATA" ] && [ -n "$(ls -A "$PGDATA" 2>/dev/null)" ]; then
        echo "[pg] cleaning leftover partial PGDATA"
        rm -rf "${PGDATA:?}/"* 2>/dev/null || true
    fi

    PWFILE="$(mktemp)"
    printf '%s\n' "${APP_PG_ADMIN_PASSWORD:-$POSTGRES_PASSWORD}" > "$PWFILE"
    chmod 600 "$PWFILE"
    # initdb 失败 → 退出非零 → supervisor autorestart 重试 (上面清理保证幂等)
    if ! "$PG_BIN/initdb" -D "$PGDATA" \
            --username="$PG_ADMIN_USER" --pwfile="$PWFILE" \
            --auth-host=scram-sha-256 --auth-local=trust; then
        rm -f "$PWFILE"
        echo "[pg] initdb failed, will retry on supervisor autorestart" >&2
        exit 1
    fi
    rm -f "$PWFILE"
    pg_admin_identity_record || exit 1

    echo "[pg] initdb done"
fi

# Verify directory permissions before starting either owned child.
chmod 700 "$PGDATA" || exit 1
# Python comes with the image's existing supervisor package. One owner stops
# both process groups; the bootstrap task never signals a stale parent PID.
export PG_BIN PGDATA POSTGRES_USER POSTGRES_PASSWORD POSTGRES_DB
exec /usr/bin/python3 /usr/local/bin/pg-supervise.py /usr/local/bin/pg-admin-identity.sh

#!/bin/sh
# =============================================================================
# supervisor `program:postgresql` 入口（由 supervisor 以 user=postgres 运行）。
#
# 为什么存在: PG 首次 initdb 必须异步于 agent_runner。旧版 init_db 在 start-up.sh
# 里同步阻塞, 而 PGDATA 落 CephFS → initdb >60s → agent_runner 的 :8086/health
# 在 liveness 窗口内起不来 → 容器被杀 (exit 137) → restartPolicy=Never 永久 Error。
# agent_runner 不依赖 PG (PG 是给用户开发用的本地库), 故把 initdb 挪到这里、由
# supervisor 托管, initdb 再慢也只推迟 PG 可用, 不影响 :8086 health。
#
# 幂等: PG_VERSION 缺失才 initdb; supervisor autorestart 重试安全。
# 末尾由进程所有者统一托管 postgres 和初始化任务，并转发停止信号。
# =============================================================================
set -u

PG_BIN=/usr/lib/postgresql/16/bin
: "${PGDATA:=/home/user/.pgdata}"
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

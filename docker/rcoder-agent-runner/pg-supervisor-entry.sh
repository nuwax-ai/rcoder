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
# 末尾 exec postgres 前台 → supervisor 直接追踪 postgres PID (与旧 postgres.conf
# 直接跑 postgres 的行为一致, 无需 gosu/su-exec)。
# =============================================================================
set -u

PG_BIN=/usr/lib/postgresql/16/bin
: "${PGDATA:=/home/user/.pgdata}"
: "${POSTGRES_USER:=dev}"
: "${POSTGRES_PASSWORD:=dev}"
: "${POSTGRES_DB:=dev}"

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
# chmod 700: postgres 要求 PGDATA u=rwx(0700)或 u=rwx,g=rx(0750); start-up.sh 的 install -d 默认建 755
# → postgres FATAL "data directory has invalid permissions" → 数据库客户端连不上 PG. 幂等兜底 (supervisor 每次重启都校正, 也防 fsGroup/PVC 改权限).
chmod 700 "$PGDATA" 2>/dev/null || true
exec "$PG_BIN/postgres" -D "$PGDATA"

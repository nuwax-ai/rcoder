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

    # 业务库兜底改后台幂等建库（postgres 正式起来后补建）：原"临时起停 +
    # createdb"在慢盘下 -w 超时被 || true 静默吞错 → 业务库永久缺失
    # （pg_isready 通但业务库 FATAL not exist 的中间态）。挪到 exec postgres
    # 之后由后台循环等 socket 就绪再幂等 createdb（已存在自动跳过）。
    echo "[pg] initdb done (user=$POSTGRES_USER db=$POSTGRES_DB)"
fi

# 后台幂等建库（无论是否首次 init 都兜底——覆盖历史半成品/临时起停失败遗留）
(
    for _i in $(seq 1 300); do
        "$PG_BIN/psql" -h /var/run/postgresql -U "$POSTGRES_USER" \
            -d postgres -tAc "select 1" >/dev/null 2>&1 && break
        sleep 1
    done
    if ! "$PG_BIN/createdb" -h /var/run/postgresql -U "$POSTGRES_USER" \
            "$POSTGRES_DB" 2>/dev/null; then
        echo "[pg] createdb $POSTGRES_DB failed or already exists (idempotent skip)"
    else
        echo "[pg] business database $POSTGRES_DB created"
    fi
) &

# 前台运行 postgres, 供 supervisor 直接托管 (PID 即 postgres 本体)
# chmod 700: postgres 要求 PGDATA u=rwx(0700)或 u=rwx,g=rx(0750); start-up.sh 的 install -d 默认建 755
# → postgres FATAL "data directory has invalid permissions" → 数据库客户端连不上 PG. 幂等兜底 (supervisor 每次重启都校正, 也防 fsGroup/PVC 改权限).
chmod 700 "$PGDATA" 2>/dev/null || true
exec "$PG_BIN/postgres" -D "$PGDATA"

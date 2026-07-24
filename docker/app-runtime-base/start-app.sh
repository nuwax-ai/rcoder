#!/bin/bash
# ============================================================================
# app-runtime ENTRYPOINT —— supervisor 管 PG + pgweb + ttyd + 用户应用
# 用户 command(CMD args / $@)动态生成 [program:app],随容器启动
# supervisor 作 PID 1:docker stop SIGTERM → 优雅停 PG(INT 信号)不丢数据
# ============================================================================
set -e

export PGDATA="${PGDATA:-/app/data/pg}"
export POSTGRES_USER="${POSTGRES_USER:-app}"
export POSTGRES_PASSWORD="${POSTGRES_PASSWORD:-app}"
export POSTGRES_DB="${POSTGRES_DB:-app}"
export PGWEB_PORT="${PGWEB_PORT:-8081}"

mkdir -p /app/data /app/logs /app/config /app/code
# PGDATA 属主备好(非递归, 瞬时)。PGDATA 落 /app/data(PVC), 首次 initdb 慢。
# postgres 需 traverse /app/data → 单层 chown(非 -R, 避免递归扫 PVC 慢)。
chown postgres:postgres /app/data 2>/dev/null || true
install -d -o postgres -g postgres "$PGDATA"

# ============================================================================
# 1. PostgreSQL 首次 initdb 已异步化 —— 由 supervisor 托管的
#    /usr/local/bin/pg-supervisor-entry.sh 在 [program:postgresql] 里幂等执行
#    (PG_VERSION 缺失才 initdb)。不再在此同步阻塞 exec supervisord, 避免
#    UserApp 首启慢被 liveness 杀(restartPolicy=Always → CrashLoopBackOff)。
#    PGDATA 在 /app/data(PVC), 重启不丢数据。
# ============================================================================

# 连接信息供用户参考(pgweb UI 手填 / 应用连接)
cat > /app/config/pg-connection.txt <<EOF
PostgreSQL 连接信息:
  容器内:host=localhost port=5432 user=$POSTGRES_USER password=$POSTGRES_PASSWORD dbname=$POSTGRES_DB sslmode=disable
  K8s 集群内:host=app-{app_id}-svc port=5432 (同上凭证)
EOF

# ============================================================================
# 2. 生成用户应用的 supervisor program(动态 command)
#    $@ = docker CMD args(UserApp 的用户 command)
# ============================================================================
APP_CONF=/etc/supervisor/conf.d/99-app.conf
if [ $# -gt 0 ]; then
    # printf %q 转义每个 arg —— supervisor shlex 正确解析含空格/特殊字符的 command
    ESCAPED=""
    for arg in "$@"; do
        ESCAPED+="$(printf '%q ' "$arg")"
    done
    cat > "$APP_CONF" <<EOF
[program:app]
command=$ESCAPED
directory=/app/code
priority=40
autostart=true
autorestart=true
startsecs=5
stdout_logfile=/app/logs/app.out.log
stderr_logfile=/app/logs/app.err.log
EOF
    echo "🚀 User app registered: $ESCAPED"
else
    cat > "$APP_CONF" <<EOF
[program:app]
command=sleep infinity
autostart=false
autorestart=false
EOF
    echo "⚠️ No user command; running PG/pgweb/ttyd only"
fi

# ============================================================================
# 3. 启动 supervisor(前台 PID 1)
# ============================================================================
exec supervisord -n -c /etc/supervisor/supervisord.conf

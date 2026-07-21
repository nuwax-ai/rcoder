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

mkdir -p /app/data "$PGDATA" /app/logs /app/config /app/code

# ============================================================================
# 1. 初始化 PostgreSQL(首次,PGDATA 为空)
# ============================================================================
if [ ! -s "$PGDATA/PG_VERSION" ]; then
    echo "📦 Initializing PostgreSQL at $PGDATA ..."
    chown -R postgres:postgres /app/data
    PWFILE=$(mktemp)
    printf '%s\n' "$POSTGRES_PASSWORD" > "$PWFILE"
    chown postgres:postgres "$PWFILE"
    chmod 600 "$PWFILE"
    su postgres -c "/usr/lib/postgresql/16/bin/initdb -D \"$PGDATA\" --username=\"$POSTGRES_USER\" --pwfile=\"$PWFILE\" --auth-host=scram-sha-256 --auth-local=trust"
    rm -f "$PWFILE"
    # 临时启动(仅 unix socket)建业务库
    su postgres -c "/usr/lib/postgresql/16/bin/pg_ctl -D \"$PGDATA\" -o '-c listen_addresses= -c unix_socket_directories=/tmp' -w start"
    su postgres -c "/usr/lib/postgresql/16/bin/createdb -h /tmp -U \"$POSTGRES_USER\" \"$POSTGRES_DB\"" 2>/dev/null || true
    su postgres -c "/usr/lib/postgresql/16/bin/pg_ctl -D \"$PGDATA\" -m fast -w stop"
    echo "✅ PostgreSQL initialized (user=$POSTGRES_USER db=$POSTGRES_DB)"
fi
chown -R postgres:postgres /app/data 2>/dev/null || true

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

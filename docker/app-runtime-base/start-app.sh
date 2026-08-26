#!/bin/bash
# ============================================================================
# app-runtime ENTRYPOINT —— supervisor 管 PG + pgweb + ttyd + app-cli
# app-cli 从 $WS/code/release.lock.toml 编排 workspace 内的全部用户服务与 Pingap。
# supervisor 作 PID 1:docker stop SIGTERM → 优雅停 PG(INT 信号)不丢数据
#
# 路径契约（prod 四目录压平挂载, 与 dev 开发容器同图）:
#   WS=$USERAPP_WORKSPACE_DIR (rcoder 创建容器时注入 /home/user/{app_id};
#   本地直跑未注入时缺省回退 /app)、数据=/home/user/data、日志=/home/user/logs。
# ============================================================================
set -e

# workspace 根（发布代码根;镜像 Dockerfile ENV 与本兜底双保险,容器注入覆盖）
WS="${USERAPP_WORKSPACE_DIR:-/app}"

export PGDATA="${PGDATA:-/home/user/data/pg}"
export POSTGRES_USER="${POSTGRES_USER:-app}"
export POSTGRES_PASSWORD="${POSTGRES_PASSWORD:-app}"
export POSTGRES_DB="${POSTGRES_DB:-app}"
export PGWEB_PORT="${PGWEB_PORT:-8081}"

# dbx-web(DBX 数据库 Web GUI):默认配置导出,supervisor [program:dbx] 继承。
# 密码不设默认 → 首访浏览器自设(存 $DBX_DATA_DIR/dbx.db);需固定密码时注入 DBX_PASSWORD env。
export DBX_PORT="${DBX_PORT:-4224}"
export DBX_DATA_DIR="${DBX_DATA_DIR:-/home/user/data/dbx}"
export DBX_STATIC_DIR="${DBX_STATIC_DIR:-/usr/local/share/dbx/static}"
# 首次播种本地 PG 连接:dbx-web 启动时每次免认证导入 $DBX_DATA_DIR/connections.json
# (导入后改名 .bak;fork dbx 为按 id upsert 吸收——文件存在=同步意图,rcoder
# 改密链 align/reset-password 重写本文件+重启 dbx 即完成 local-pg 凭据同步;
# 字段 snake_case 见 dbx-core ConnectionConfigData)
if [ ! -e "$DBX_DATA_DIR/dbx.db" ] && [ ! -e "$DBX_DATA_DIR/connections.json" ]; then
    mkdir -p "$DBX_DATA_DIR"
    printf '[{"id":"local-pg","name":"Local PostgreSQL","db_type":"postgres","host":"127.0.0.1","port":5432,"username":"%s","password":"%s","database":"%s"}]\n' \
        "$POSTGRES_USER" "$POSTGRES_PASSWORD" "$POSTGRES_DB" > "$DBX_DATA_DIR/connections.json"
fi

mkdir -p "$WS/code" "$WS/config" /home/user/data /home/user/logs
# PGDATA 属主备好(非递归, 瞬时)。PGDATA 落 /home/user/data(挂载卷), 首次 initdb 慢。
# postgres 需 traverse /home/user/data → 单层 chown(非 -R, 避免递归扫卷慢)。
chown postgres:postgres /home/user/data 2>/dev/null || true
install -d -o postgres -g postgres "$PGDATA"

# ============================================================================
# 1. PostgreSQL 首次 initdb 已异步化 —— 由 supervisor 托管的
#    /usr/local/bin/pg-supervisor-entry.sh 在 [program:postgresql] 里幂等执行
#    (PG_VERSION 缺失才 initdb)。不再在此同步阻塞 exec supervisord, 避免
#    UserApp 首启慢被 liveness 杀(restartPolicy=Always → CrashLoopBackOff)。
#    PGDATA 在 /home/user/data(挂载卷), 重启不丢数据。
# ============================================================================

# 连接信息供用户参考(pgweb UI 手填 / 应用连接)
cat > "$WS/config/pg-connection.txt" <<EOF
PostgreSQL 连接信息:
  容器内:host=localhost port=5432 user=$POSTGRES_USER password=$POSTGRES_PASSWORD dbname=$POSTGRES_DB sslmode=disable
  K8s 集群内:host=app-{app_id}-svc port=5432 (同上凭证)
EOF

# ============================================================================
# 2. 注册 app-cli server。
#    常驻 server 形态（serve 子命令）：无论是否部署都在（无部署=Idle 态照常
#    应答 :3010 探针，空容器不 CrashLoop）；用户服务由 server 经 supervisord
#    XML-RPC 注册为动态 program（app-svc-* / app-pingap，见 conf.d/50 分片），
#    per-service 隔离重启。部署三元组 env（APP_DEPLOY_URL 等）与热部署令牌
#    （APP_CLI_DEPLOY_TOKEN）由 rcoder 注入，此处不干预。
# ============================================================================
APP_CONF=/etc/supervisor/conf.d/99-app.conf
cat > "$APP_CONF" <<EOF
[program:app-cli]
command=/usr/local/bin/app-cli serve
directory=$WS/code
priority=40
autostart=true
autorestart=true
startsecs=5
startretries=10
stopsignal=TERM
stopasgroup=true
killasgroup=true
stopwaitsecs=45
environment=APP_CLI_WORKSPACE="$WS/code",APP_CLI_LOG_DIR="/home/user/logs"
stdout_logfile=/home/user/logs/app-cli.out.log
stderr_logfile=/home/user/logs/app-cli.err.log
EOF
echo "🚀 app-cli server registered (workspace=$WS/code)"

# ============================================================================
# 3. 启动 supervisor(前台 PID 1)
# ============================================================================
exec supervisord -n -c /etc/supervisor/supervisord.conf

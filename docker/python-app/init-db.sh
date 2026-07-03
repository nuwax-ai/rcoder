#!/bin/bash
set -e

# 颜色输出
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

# 环境变量
POSTGRES_USER=${POSTGRES_USER:-appuser}
POSTGRES_PASSWORD=${POSTGRES_PASSWORD:-apppassword}
POSTGRES_DB=${POSTGRES_DB:-appdb}
PG_VERSION=15

log_info "检查 PostgreSQL 数据目录..."

# 检查是否需要初始化
if [ ! -f "/app/data/postgresql/PG_VERSION" ]; then
    log_info "初始化 PostgreSQL 数据目录..."
    su - postgres -c "/usr/lib/postgresql/${PG_VERSION}/bin/initdb -D /app/data/postgresql"
fi

# 启动 PostgreSQL 临时服务进行初始化
log_info "启动 PostgreSQL 临时服务..."
su - postgres -c "/usr/lib/postgresql/${PG_VERSION}/bin/pg_ctl -D /app/data/postgresql -l /var/log/postgresql.log start"

# 等待 PostgreSQL 启动
sleep 3

# 创建用户和数据库
log_info "创建数据库用户和数据库..."
su - postgres -c "psql -tc \"SELECT 1 FROM pg_roles WHERE rolname='${POSTGRES_USER}'\" | grep -q 1" || \
    su - postgres -c "psql -c \"CREATE USER ${POSTGRES_USER} WITH PASSWORD '${POSTGRES_PASSWORD}';\""

su - postgres -c "psql -tc \"SELECT 1 FROM pg_database WHERE datname='${POSTGRES_DB}'\" | grep -q 1" || \
    su - postgres -c "psql -c \"CREATE DATABASE ${POSTGRES_DB} OWNER ${POSTGRES_USER};\""

su - postgres -c "psql -c \"GRANT ALL PRIVILEGES ON DATABASE ${POSTGRES_DB} TO ${POSTGRES_USER};\""

# 停止临时服务
log_info "停止 PostgreSQL 临时服务..."
su - postgres -c "/usr/lib/postgresql/${PG_VERSION}/bin/pg_ctl -D /app/data/postgresql stop"

log_info "PostgreSQL 初始化完成"

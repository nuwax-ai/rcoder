#!/bin/bash
set -e

# 颜色输出
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# 环境变量默认值
POSTGRES_USER=${POSTGRES_USER:-appuser}
POSTGRES_PASSWORD=${POSTGRES_PASSWORD:-apppassword}
POSTGRES_DB=${POSTGRES_DB:-appdb}
APP_ENV=${APP_ENV:-production}
ETCD_NAME=${ETCD_NAME:-etcd-node-1}

log_info "启动 Go + PostgreSQL + Node + etcd 应用容器..."

# 创建 etcd 数据目录
mkdir -p /app/data/etcd
chmod 700 /app/data/etcd

# 初始化数据库
log_info "初始化 PostgreSQL..."
/app/init-db.sh

# 创建日志目录
mkdir -p /var/log/supervisor

# 检查 Go 源文件
if [ ! -f "/app/go.mod" ]; then
    log_warn "未找到 go.mod 文件"
    log_warn "请将 Go 项目挂载到 /app/ 目录"
fi

# 编译 Go 应用（如果是开发模式）
if [ "$APP_ENV" = "development" ] && [ -f "/app/go.mod" ]; then
    log_info "编译 Go 应用 (开发模式)..."
    cd /app && go build -o bin/app ./...
elif [ ! -f "/app/bin/app" ] && [ -f "/app/go.mod" ]; then
    log_info "编译 Go 应用..."
    cd /app && go build -o bin/app ./...
fi

# 检查编译产物
if [ ! -f "/app/bin/app" ]; then
    log_warn "未找到 Go 编译产物 (/app/bin/app)"
    log_warn "请确保应用已编译或挂载编译后的二进制文件"
fi

# 安装依赖（如果存在 package.json）
if [ -f "/app/src/node/package.json" ]; then
    log_info "安装 Node.js 依赖..."
    cd /app/src/node && npm install
fi

# 检查 Node.js 应用文件
if [ ! -f "/app/src/node/server.js" ]; then
    log_warn "未找到 Node.js 应用文件 (/app/src/node/server.js)"
    log_warn "请将 Node.js 应用挂载到 /app/src/node/ 目录"
fi

# 等待 etcd 启动
log_info "等待 etcd 启动..."
for i in {1..30}; do
    if etcdctl endpoint health --endpoints=http://localhost:2379 2>/dev/null; then
        log_info "etcd 已就绪"
        break
    fi
    sleep 1
done

# 启动所有服务
log_info "启动 supervisord 管理所有服务..."
exec /usr/bin/supervisord -c /etc/supervisor/conf.d/supervisord.conf

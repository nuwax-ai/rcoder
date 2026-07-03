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
PYTHONPATH=${PYTHONPATH:-/app/src}
ETCD_NAME=${ETCD_NAME:-etcd-node-1}

log_info "启动 Python + PostgreSQL + Node + etcd 应用容器..."

# 创建 etcd 数据目录
mkdir -p /app/data/etcd
chmod 700 /app/data/etcd

# 初始化数据库
log_info "初始化 PostgreSQL..."
/app/init-db.sh

# 创建日志目录
mkdir -p /var/log/supervisor

# 检查 Python 应用文件
if [ ! -f "/app/src/app.py" ] && [ ! -f "/app/src/main.py" ]; then
    log_warn "未找到 Python 应用文件 (/app/src/app.py 或 /app/src/main.py)"
    log_warn "请将 Python 应用挂载到 /app/src/ 目录"
fi

# 安装 Python 依赖（如果存在 requirements.txt）
if [ -f "/app/src/requirements.txt" ]; then
    log_info "安装 Python 依赖..."
    /app/venv/bin/pip install --no-cache-dir -r /app/src/requirements.txt
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

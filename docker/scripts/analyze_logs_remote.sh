#!/bin/bash
# 远程日志分析脚本 - 从远程服务器获取并分析日志
# 用法: ./analyze_logs_remote.sh

# 加载环境变量
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -f "$SCRIPT_DIR/.env" ]; then
  source "$SCRIPT_DIR/.env"
fi

REMOTE_HOST="${REMOTE_HOST:-192.168.1.34}"
REMOTE_USER="${REMOTE_USER:-swufe}"
REMOTE_PASS="${REMOTE_PASS:-Swufe@2024}"

# 日志路径配置
MAIN_LOG_PATH="/home/swufe/nuwax/docker/logs/rcoder/rcoder.log"
CONTAINER_LOG_DIR="/home/swufe/nuwax/docker/logs/rcoder/container"

echo "🌐 远程日志分析报告"
echo "================================================"
echo "服务器: ${REMOTE_USER}@${REMOTE_HOST}"
echo "主服务日志: ${MAIN_LOG_PATH}"
echo "子容器日志目录: ${CONTAINER_LOG_DIR}"
echo ""

# 使用 sshpass 进行远程连接并分析日志
if ! command -v sshpass &> /dev/null; then
    echo "⚠️  sshpass 未安装"
    echo "   请执行: brew install hudochenkov/sshpass/sshpass"
    echo ""
    echo "或手动连接服务器查看日志:"
    echo "   ssh ${REMOTE_USER}@${REMOTE_HOST}"
    echo "   密码: ${REMOTE_PASS}"
    echo ""
    echo "常用日志命令:"
    echo "   # 查看主服务日志"
    echo "   tail -100 ${MAIN_LOG_PATH}"
    echo ""
    echo "   # 查看子容器数量"
    echo "   ls -la ${CONTAINER_LOG_DIR} | wc -l"
    echo ""
    echo "   # 查看最新子容器日志"
    echo "   ls -t ${CONTAINER_LOG_DIR} | head -1 | xargs -I {} cat ${CONTAINER_LOG_DIR}/{}/startup.log"
    exit 0
fi

SSH_CMD="sshpass -p '${REMOTE_PASS}' ssh -o StrictHostKeyChecking=no ${REMOTE_USER}@${REMOTE_HOST}"

echo "📊 运行中的子容器:"
eval "${SSH_CMD} 'docker ps --filter \"name=computer-agent\" --format \"table {{.ID}}\t{{.Names}}\t{{.Status}}\" 2>/dev/null | head -20'"
echo ""

echo "📈 子容器统计:"
CONTAINER_COUNT=$(eval "${SSH_CMD} 'docker ps --filter \"name=computer-agent\" -q 2>/dev/null | wc -l'")
LOG_DIR_COUNT=$(eval "${SSH_CMD} 'ls -d ${CONTAINER_LOG_DIR}/*/ 2>/dev/null | wc -l'")
echo "  运行中容器: ${CONTAINER_COUNT} 个"
echo "  日志目录数: ${LOG_DIR_COUNT} 个"
echo ""

echo "📝 主服务最近日志 (最后 30 行):"
eval "${SSH_CMD} 'tail -30 ${MAIN_LOG_PATH} 2>/dev/null'" || echo "  无法读取日志"
echo ""

echo "🔍 主服务错误日志:"
eval "${SSH_CMD} 'grep -iE \"(error|Error|ERROR|失败|超时|panic|fatal)\" ${MAIN_LOG_PATH} 2>/dev/null | tail -10'" || echo "  无错误日志"
echo ""

echo "📋 最新子容器日志目录:"
LATEST_CONTAINER=$(eval "${SSH_CMD} 'ls -t ${CONTAINER_LOG_DIR} 2>/dev/null | head -1'")
if [ -n "$LATEST_CONTAINER" ] && [ "$LATEST_CONTAINER" != "" ]; then
    echo "  目录: ${CONTAINER_LOG_DIR}/${LATEST_CONTAINER}"
    echo ""
    echo "📄 子容器启动日志 (最新):"
    eval "${SSH_CMD} 'cat ${CONTAINER_LOG_DIR}/${LATEST_CONTAINER}/startup.log 2>/dev/null | tail -40'" || echo "  无法读取日志"
else
    echo "  没有子容器日志目录"
fi
echo ""

echo "🚨 子容器错误日志:"
if [ -n "$LATEST_CONTAINER" ] && [ "$LATEST_CONTAINER" != "" ]; then
    eval "${SSH_CMD} 'grep -iE \"(error|Error|ERROR|失败)\" ${CONTAINER_LOG_DIR}/${LATEST_CONTAINER}/startup.log 2>/dev/null | tail -10'" || echo "  无错误日志"
else
    echo "  没有子容器日志"
fi
echo ""

echo "🕵️‍♀️ MCP 初始化超时分析:"
echo "正在扫描所有子容器日志，查找 '[ACP] new_session 超时'..."
TIMEOUT_COUNT=$(eval "${SSH_CMD} 'grep -r \"new_session 超时\" ${CONTAINER_LOG_DIR} 2>/dev/null | wc -l'")
PROTECT_COUNT=$(eval "${SSH_CMD} 'grep -r \"已启用 100 秒超时保护\" ${CONTAINER_LOG_DIR} 2>/dev/null | wc -l'")

echo "  ⛔️ 发生超时的容器数: ${TIMEOUT_COUNT}"
echo "  ⚠️ 启用超时保护的容器数: ${PROTECT_COUNT}"
echo ""
if [ "$TIMEOUT_COUNT" -gt 0 ]; then
    echo "  示例超时日志:"
    eval "${SSH_CMD} 'grep -r \"new_session 超时\" ${CONTAINER_LOG_DIR} 2>/dev/null | head -3'"
fi
echo ""

echo "================================================"
echo "💡 分析建议:"
echo "  1. 日志目录数 > 运行中容器数 说明有容器已退出但日志保留"
echo "  2. 查看主服务日志中的 error/超时 信息"
echo "  3. 检查子容器启动日志中的 MCP 初始化状态"
echo ""
echo "📋 手动查看命令:"
echo "   ssh ${REMOTE_USER}@${REMOTE_HOST}"
echo "   密码: ${REMOTE_PASS}"
echo ""
echo "   # 主服务日志"
echo "   tail -f ${MAIN_LOG_PATH}"
echo ""
echo "   # 列出所有子容器日志"
echo "   ls -lt ${CONTAINER_LOG_DIR}"
echo ""
echo "   # 查看特定容器日志"
echo "   cat ${CONTAINER_LOG_DIR}/<container-name>/startup.log"

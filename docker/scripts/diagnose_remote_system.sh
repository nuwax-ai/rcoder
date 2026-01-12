#!/bin/bash
# -----------------------------------------------------------------------------
# 远程系统诊断脚本 (diagnose_remote_system.sh)
# 用途: 自动诊断远程服务器上的 rcoder 服务状态，重点检测 MCP 超时、API 限流和容器积压问题。
# -----------------------------------------------------------------------------

# --- 配置加载 ---
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -f "$SCRIPT_DIR/.env" ]; then
  source "$SCRIPT_DIR/.env"
fi

REMOTE_HOST="${REMOTE_HOST:-192.168.1.34}"
REMOTE_USER="${REMOTE_USER:-swufe}"
REMOTE_PASS="${REMOTE_PASS:-Swufe@2024}"
# 日志路径
MAIN_LOG_PATH="/home/swufe/nuwax/docker/logs/rcoder/rcoder.log"
CONTAINER_LOG_DIR="/home/swufe/nuwax/docker/logs/rcoder/container"

# --- 辅助函数 ---
check_sshpass() {
    if ! command -v sshpass &> /dev/null; then
        echo "❌ 错误: sshpass 未安装。"
        echo "   请运行: brew install hudochenkov/sshpass/sshpass"
        exit 1
    fi
}

run_remote() {
    local cmd="$1"
    sshpass -p "${REMOTE_PASS}" ssh -o StrictHostKeyChecking=no "${REMOTE_USER}@${REMOTE_HOST}" "$cmd" 2>/dev/null
}

print_header() {
    echo ""
    echo "========================================================"
    echo "🩺 $1"
    echo "========================================================"
}

# --- 主逻辑 ---
check_sshpass
TARGET_BATCH_ID="$1"

if [ -n "$TARGET_BATCH_ID" ]; then
    print_header "批次分析模式: ${TARGET_BATCH_ID}"
    echo "🔍 正在查找与批次 ID 相关的容器和日志..."
    
    # 获取运行中的相关容器
    echo "📊 运行中的容器:"
    RUNNING_CONTAINERS=$(run_remote "docker ps --filter \"name=computer-agent-runner-${TARGET_BATCH_ID}\" --format \"table {{.Names}}\t{{.Status}}\"")
    if [ -n "$RUNNING_CONTAINERS" ] && [ "$RUNNING_CONTAINERS" != "NAMES   STATUS" ]; then
        echo "$RUNNING_CONTAINERS"
    else
        echo "  (无运行中容器)"
    fi
    echo ""

    # 获取日志目录（包括已退出的）
    echo "📂 相关日志文件分析:"
    # 列出所有匹配该 Batch ID 的日志目录
    LOG_DIRS=$(run_remote "ls -d ${CONTAINER_LOG_DIR}/*${TARGET_BATCH_ID}*/ 2>/dev/null")
    
    if [ -z "$LOG_DIRS" ]; then
        echo "❌ 未找到该批次的任何日志目录。"
        exit 0
    fi

    COUNT=0
    TIMEOUT_COUNT=0
    
    # 遍历每个日志目录进行分析（限制前 30 个以防太多）
    for dir in $(echo "$LOG_DIRS" | head -30); do
        DIR_NAME=$(basename "$dir")
        # 提取请求信息 (假设格式包含 user_id)
        # 格式示例: computer-agent-runner-b1768207074_r2_u14-20260112083954
        # 我们想提取 r2_u14
        REQ_INFO=$(echo "$DIR_NAME" | grep -o "r[0-9]*_u[0-9]*")
        
        # 检查超时
        TIMEOUT_LOG=$(run_remote "grep \"new_session 超时\" ${dir}/startup.log 2>/dev/null")
        
        if [ -n "$TIMEOUT_LOG" ]; then
            DURATION=$(echo "$TIMEOUT_LOG" | grep -o "耗时: [0-9.]*s")
            echo "🔴 [${REQ_INFO}] ⚠️ 超时! ${DURATION} -> ${DIR_NAME}"
            TIMEOUT_COUNT=$((TIMEOUT_COUNT + 1))
        else
            # 检查是否有成功启动日志
            SUCCESS=$(run_remote "grep \"MCP 服务器数量\" ${dir}/startup.log 2>/dev/null")
            if [ -n "$SUCCESS" ]; then
                echo "✅ [${REQ_INFO}] 正常启动 -> ${DIR_NAME}"
            else
                # 检查是否有其他错误
                ERROR=$(run_remote "grep -iE \"(error|panic)\" ${dir}/startup.log 2>/dev/null | head -1")
                if [ -n "$ERROR" ]; then
                   echo "❌ [${REQ_INFO}] 启动失败: $(echo $ERROR | cut -c 1-50)..."
                else
                   echo "❓ [${REQ_INFO}] 状态未知 (日志可能不完整)"
                fi
            fi
        fi
        COUNT=$((COUNT + 1))
    done

    echo ""
    echo "------------------------------------------------"
    echo "🔍 正在检查主服务日志 (rcoder.log) 中的相关错误..."
    if [ -n "$BATCH_ID" ]; then
        # Check for timeouts in main log correlated with the Batch ID
        # Specifically looking for "[ACP] new_session 超时" as requested
        echo "检查项 1: [ACP] new_session 超时"
        sshpass -p "$REMOTE_PASS" ssh -o StrictHostKeyChecking=no $REMOTE_USER@$REMOTE_HOST \
            "grep '$BATCH_ID' $REMOTE_MAIN_LOG | grep '\[ACP\] new_session 超时' | tail -n 5"

        echo "检查项 2: 智能体初始化超时 (通用消息)"
        sshpass -p "$REMOTE_PASS" ssh -o StrictHostKeyChecking=no $REMOTE_USER@$REMOTE_HOST \
            "grep '$BATCH_ID' $REMOTE_MAIN_LOG | grep '智能体初始化超时' | tail -n 5"
    fi

    echo "统计: 分析了 $COUNT 个日志目录, 发现 $TIMEOUT_COUNT 个超时。"
    exit 0
fi

# ========================================================
# 默认模式 (无参数)
# ========================================================

echo "🌐 连接到服务器: ${REMOTE_USER}@${REMOTE_HOST} ..."

# 1. 系统概览
print_header "系统健康概览"
DOCKER_PS=$(run_remote "docker ps --filter \"name=computer-agent\" --format \"{{.ID}}\"")
RUNNING_COUNT=$(echo "$DOCKER_PS" | grep -v "^$" | wc -l | xargs)
LOG_DIRS=$(run_remote "ls -d ${CONTAINER_LOG_DIR}/*/ 2>/dev/null | wc -l" | xargs)

echo "✅ 运行中的子容器 (Agents) : ${RUNNING_COUNT}"
echo "📁 已存留的日志目录        : ${LOG_DIRS}"

if [ "$RUNNING_COUNT" -gt 20 ]; then
    echo "⚠️  警告: 运行中容器数量较高 (>20)，关注资源负载。"
fi
if [ "$LOG_DIRS" -gt "$((RUNNING_COUNT + 10))" ]; then
    echo "⚠️  警告: 发现大量残留日志目录，可能存在已退出但未清理的容器记录。"
fi

# 2. MCP 初始化超时分析
print_header "MCP 初始化超时分析 (100s Timeout)"
TIMEOUT_COUNT=$(run_remote "grep -r \"new_session 超时\" ${CONTAINER_LOG_DIR} 2>/dev/null | wc -l" | xargs)
PROTECT_COUNT=$(run_remote "grep -r \"已启用 100 秒超时保护\" ${CONTAINER_LOG_DIR} 2>/dev/null | wc -l" | xargs)

echo "🔴 发生 'new_session 超时' 次数 : ${TIMEOUT_COUNT}"
echo "🛡️  触发 '100s 超时保护' 次数   : ${PROTECT_COUNT}"

if [ "$TIMEOUT_COUNT" -gt 0 ]; then
    echo ""
    echo "🔍 最近的超时日志示例:"
    run_remote "grep -r \"new_session 超时\" ${CONTAINER_LOG_DIR} 2>/dev/null | tail -3"
    echo ""
    echo "👉 建议: 减少单次请求启用的 MCP 工具数量或复用容器。"
else
    echo "✅ 未检测到 MCP 初始化超时。"
fi

# 3. API 限流分析
print_header "LLM API 限流分析 (429 Errors)"
RATELIMIT_COUNT=$(run_remote "grep -c \"429 Too Many Requests\" ${MAIN_LOG_PATH} 2>/dev/null" | xargs)

echo "⛔️ '429 Too Many Requests' 总数 : ${RATELIMIT_COUNT}"

if [ "$RATELIMIT_COUNT" -gt 0 ]; then
    echo ""
    echo "🔍 最近的 429 错误日志:"
    run_remote "grep \"429 Too Many Requests\" ${MAIN_LOG_PATH} 2>/dev/null | tail -3"
    echo ""
    echo "👉 建议: 降低并发数或更换更高配额的 API Key。"
else
    echo "✅ 未检测到 API 限流错误。"
fi

# 4. 其他错误摘要
print_header "其他关键错误摘要"
echo "正在扫描主服务日志中的其他 Error/Panic..."
ERROR_LOGS=$(run_remote "grep -iE \"(panic|fatal|error)\" ${MAIN_LOG_PATH} 2>/dev/null | grep -v \"429 Too Many Requests\" | tail -5")

if [ -n "$ERROR_LOGS" ]; then
    echo "$ERROR_LOGS"
else
    echo "✅ 主服务日志中未发现其他显著错误。"
fi

echo ""
echo "========================================================"
echo "诊断完成 (运行 ./diagnose_remote_system.sh [BATCH_ID] 可针对特定批次分析)。"


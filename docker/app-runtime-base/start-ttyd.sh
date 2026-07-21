#!/bin/bash
# ============================================================================
# ttyd 启动脚本 - 应用容器基础镜像
# ============================================================================

set -e

PORT="${TTYD_PORT:-7681}"
USER_NAME="${TTYD_USER:-root}"
CREDENTIAL="${TTYD_CREDENTIAL:-}"

# 凭据（可选）
AUTH_OPT=""
if [ -n "$CREDENTIAL" ]; then
    AUTH_OPT="-c ${CREDENTIAL}"
fi

# 创建 wrapper 脚本
WRAPPER="/tmp/ttyd-wrapper.sh"
cat > "${WRAPPER}" <<'EOF'
#!/bin/bash
export HOME="/app"

# 解析 --cwd 参数
TARGET_DIR=""
while [ $# -gt 0 ]; do
    case "$1" in
        --cwd) TARGET_DIR="$2"; shift 2 ;;
        *) shift ;;
    esac
done

# 安全校验：只允许 /app/ 下的子目录
if [ -n "$TARGET_DIR" ] && [ -d "$TARGET_DIR" ]; then
    REAL_DIR=$(realpath "$TARGET_DIR" 2>/dev/null)
    case "$REAL_DIR" in
        /app|/app/*)
            cd "$REAL_DIR" 2>/dev/null || cd /app 2>/dev/null || true
            ;;
        *)
            cd /app 2>/dev/null || true
            ;;
    esac
else
    cd /app 2>/dev/null || true
fi

exec bash
EOF
chmod +x "${WRAPPER}"

echo "🚀 Starting ttyd on port ${PORT}..."
echo "   WebSocket: ws://localhost:${PORT}/ws"

exec ttyd \
    -p "${PORT}" \
    -W \
    -a \
    -w "/app" \
    ${AUTH_OPT} \
    "${WRAPPER}"

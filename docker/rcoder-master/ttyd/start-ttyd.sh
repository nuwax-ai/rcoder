#!/bin/bash
# ============================================================================
# ttyd 启动脚本 - rcoder-agent-runner 主镜像 ttyd 模块
# ============================================================================
# 文件位置：ttyd/start-ttyd.sh（被 Dockerfile COPY 注入主镜像）
# 镜像内路径：/usr/local/bin/start-ttyd.sh
# 区别于 demo/start-ttyd.sh（那个是给独立 demo 镜像用的，默认用户 demo）
# 用法：
#   默认：直接 /usr/local/bin/start-ttyd.sh
#   自定义：docker run -e TTYD_PORT=7777 -e TTYD_CREDENTIAL=user:pass ...
# ============================================================================

set -e

PORT="${TTYD_PORT:-7681}"
# 主镜像是 user (uid 1000)，不是 demo；保持和项目其他服务一致
USER_NAME="${TTYD_USER:-user}"
INDEX_PATH="${TTYD_INDEX:-/usr/local/share/ttyd/index.html}"

# 凭据（可选）：格式 user:password
# 留空 = 无认证（仅用于内网/受控环境）
CREDENTIAL="${TTYD_CREDENTIAL:-}"

# ttyd -u/-g 接受数字 ID，不接受用户名；自动转换
# 如果指定的用户不存在，自动回退到 root 用户
USER_ID="$(id -u "${USER_NAME}" 2>/dev/null || true)"
GROUP_ID="$(id -g "${USER_NAME}" 2>/dev/null || true)"
if [ -z "${USER_ID}" ] || [ -z "${GROUP_ID}" ]; then
    echo "⚠️  用户 ${USER_NAME} 不存在，使用 root 用户"
    USER_NAME="root"
    USER_ID=0
    GROUP_ID=0
fi

AUTH_OPT=""
if [ -n "$CREDENTIAL" ]; then
    AUTH_OPT="-c ${CREDENTIAL}"
    echo "🔐 启用 Basic Auth: ${CREDENTIAL%%:*} / ********"
else
    echo "⚠️  未启用认证（仅适用于受控内网环境）"
fi

echo ""
echo "🚀 ttyd 启动中..."
echo "   端口:   ${PORT}"
echo "   用户:   ${USER_NAME} (uid=${USER_ID}, gid=${GROUP_ID})"
echo "   命令:   bash (via wrapper)"
echo "   静态:   ${INDEX_PATH}"
echo ""
echo "🌐 访问方式："
echo "   1. 浏览器 UI:  http://localhost:${PORT}/"
echo "   2. WebSocket:  ws://localhost:${PORT}/ws  (子协议: tty)"
echo ""

# ttyd 的环境变量只有 TERM/TTYD_USER，HOME 会继承父进程（root）的 /root
# 导致 bash 启动时尝试读 /root/.bashrc 报 permission denied
# 用 wrapper 显式设 HOME 和工作目录
# 支持 --url-arg：ttyd 的 -a 选项把 URL query 参数传给子进程
# 前端连接 ws://host:7681/ws?arg=--cwd&arg=/home/user/22 时，
# wrapper 收到 --cwd /home/user/22 参数，cd 到项目目录再 exec bash
WRAPPER="/tmp/ttyd-wrapper.sh"
cat > "${WRAPPER}" <<'WRAPPER_EOF'
#!/bin/bash
export HOME="/home/USER_NAME_PLACEHOLDER"

# 解析 --cwd 参数（由 ttyd --url-arg 从 WebSocket URL query 传入）
TARGET_DIR=""
while [ $# -gt 0 ]; do
    case "$1" in
        --cwd) TARGET_DIR="$2"; shift 2 ;;
        *) shift ;;
    esac
done

# 安全校验：允许 /home/user/ 和 /app/project_workspace/ 下的子目录
if [ -n "$TARGET_DIR" ] && [ -d "$TARGET_DIR" ]; then
    REAL_DIR=$(realpath "$TARGET_DIR" 2>/dev/null)
    HOME_PREFIX="/home/USER_NAME_PLACEHOLDER"
    PROJECT_PREFIX="/app/project_workspace"
    case "$REAL_DIR" in
        "$HOME_PREFIX"|"$HOME_PREFIX"/*)
            cd "$REAL_DIR" 2>/dev/null || cd "${HOME}" 2>/dev/null || true
            ;;
        "$PROJECT_PREFIX"|"$PROJECT_PREFIX"/*)
            cd "$REAL_DIR" 2>/dev/null || cd "${HOME}" 2>/dev/null || true
            ;;
        *)
            cd "${HOME}" 2>/dev/null || true
            ;;
    esac
else
    cd "${HOME}" 2>/dev/null || true
fi

exec bash
WRAPPER_EOF
# 替换占位符为实际用户名（heredoc 用单引号阻止变量展开）
sed -i "s/USER_NAME_PLACEHOLDER/${USER_NAME}/g" "${WRAPPER}"
chmod +x "${WRAPPER}"

# 关键 flag 解释：
#   -W: 允许浏览器写 TTY（这是我们想要的）
#   -a: 允许客户端通过 URL query 参数传递命令行参数给子进程
#   -I: 使用自定义 index.html（同时也是默认根路径）
#   -6: 启用 IPv6 监听（默认关闭，但浏览器访问 localhost 优先用 IPv6）
#   -w: 设置工作目录到 user 家目录
#   -u/-g: 降权到 user (uid 1000)，防止 -W 模式下浏览器直接以 root 跑命令
exec ttyd \
    -p "${PORT}" \
    -u "${USER_ID}" \
    -g "${GROUP_ID}" \
    -W \
    -a \
    -I "${INDEX_PATH}" \
    -6 \
    -w "/home/${USER_NAME}" \
    ${AUTH_OPT} \
    "${WRAPPER}"

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

# ENABLE_TTYD 开关（迁 supervisor 后保留：false → exit 0，supervisor 认为正常退出不 restart）
if [ "${ENABLE_TTYD:-true}" != "true" ]; then
    echo "ttyd disabled (ENABLE_TTYD != true), exit 0"
    exit 0
fi

PORT="${TTYD_PORT:-7681}"
# Web 终端默认以 root 操作工作区；需要普通用户时可显式设置 TTYD_USER=user
USER_NAME="${TTYD_USER:-root}"
INDEX_PATH="${TTYD_INDEX:-/usr/local/share/ttyd/index.html}"

# 凭据（可选）：格式 user:password
# 留空 = 无认证（仅用于内网/受控环境）
CREDENTIAL="${TTYD_CREDENTIAL:-}"

# ttyd -u/-g 接受数字 ID，不接受用户名；自动转换
if ! USER_ID="$(id -u "${USER_NAME}" 2>/dev/null)" ||
   ! GROUP_ID="$(id -g "${USER_NAME}" 2>/dev/null)"; then
    echo "❌ 用户 ${USER_NAME} 不存在"
    exit 1
fi

# 读取账户实际家目录，root 是 /root，不能拼成 /home/root
USER_HOME="$(getent passwd "${USER_NAME}" | cut -d: -f6)"
if [[ "${USER_HOME}" != /* || ! -d "${USER_HOME}" ]]; then
    echo "❌ 用户 ${USER_NAME} 的家目录无效或不存在: ${USER_HOME}"
    exit 1
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

# 用 wrapper 显式设所选账户的 HOME，避免继承父进程的家目录
# 支持 --url-arg：ttyd 的 -a 选项把 URL query 参数传给子进程
# 前端连接 ws://host:7681/ws?arg=--cwd&arg=/home/user/22 时，
# wrapper 收到 --cwd /home/user/22 参数，cd 到项目目录再 exec bash
WRAPPER="/tmp/ttyd-wrapper.sh"
{
printf '#!/bin/bash\nexport HOME=%q\n' "${USER_HOME}"
cat <<'WRAPPER_EOF'

# 解析 --cwd 参数（由 ttyd --url-arg 从 WebSocket URL query 传入）
TARGET_DIR=""
while [ $# -gt 0 ]; do
    case "$1" in
        --cwd) TARGET_DIR="$2"; shift 2 ;;
        *) shift ;;
    esac
done

# URL 解码 --cwd 值：agent_runner 的 ws_terminal 对 cwd 做 percent-encode 后
# 注入 URL（ttyd 原样透传不解码——1.7.7 protocol.c 直 strdup），解码在此承担。
# 两端成对契约：编码端见 agent_runner ws_terminal::proxy::build_ttyd_url。
# 无 % 的路径（纯标识符）解码为 no-op，兼容旧二进制。
ttyd_urldecode() {
    local s="${1//+/ }"
    printf '%b' "${s//%/\\x}"
}
if [ -n "$TARGET_DIR" ]; then
    TARGET_DIR="$(ttyd_urldecode "$TARGET_DIR")"
fi

# --cwd 指定则进入项目目录，否则保留 ttyd 的初始目录 /home/user。
# 执行账户和工作目录独立：root 终端也不跳转到 /root。
if [ -n "$TARGET_DIR" ] && [ -d "$TARGET_DIR" ]; then
    cd "$TARGET_DIR" 2>/dev/null || true
fi

exec bash
WRAPPER_EOF
} > "${WRAPPER}"
chmod +x "${WRAPPER}"

# 关键 flag 解释：
#   -W: 允许浏览器写 TTY（这是我们想要的）
#   -a: 允许客户端通过 URL query 参数传递命令行参数给子进程
#   -I: 使用自定义 index.html（同时也是默认根路径）
#   -6: 启用 IPv6 监听（默认关闭，但浏览器访问 localhost 优先用 IPv6）
#   -w: 保持工作区根 /home/user，具体项目由 --cwd 指定
#   -u/-g: 使用所选账户的 uid/gid（默认 root 为 0/0）
exec ttyd \
    -p "${PORT}" \
    -u "${USER_ID}" \
    -g "${GROUP_ID}" \
    -W \
    -a \
    -I "${INDEX_PATH}" \
    -6 \
    -w /home/user \
    ${AUTH_OPT} \
    "${WRAPPER}"

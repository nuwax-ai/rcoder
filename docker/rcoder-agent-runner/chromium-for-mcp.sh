#!/bin/bash
# MCP 专用 Chromium 包装器
# 以 root 用户身份运行 Chromium，但 HOME 设置为 /home/user
# 确保能连接到 fcitx5 输入法的 D-Bus 会话

# 读取 D-Bus 会话地址
if [ -f /tmp/dbus-session-env ]; then
    source /tmp/dbus-session-env
fi

# 读取 Chromium 环境变量
if [ -f /etc/profile.d/chromium-env.sh ]; then
    source /etc/profile.d/chromium-env.sh
fi

# 读取输入法环境变量
if [ -f /etc/profile.d/ime-env.sh ]; then
    source /etc/profile.d/ime-env.sh
fi

# 使用用户主目录的 Chromium 配置（持久化）
CHROMIUM_DATA_DIR="${CHROMIUM_USER_DATA_DIR:-/home/user/.config/chromium}"

# 设置环境变量（以 root 运行，但 HOME=/home/user）
export DISPLAY=:0
export HOME=/home/user
export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS}"
export GTK_IM_MODULE=fcitx
export QT_IM_MODULE=fcitx
export XMODIFIERS=@im=fcitx
export INPUT_METHOD=fcitx
export SDL_IM_MODULE=fcitx
export GLFW_IM_MODULE=ibus
export LANG=C.UTF-8
export LC_ALL=C.UTF-8
export CHROMIUM_USER_DATA_DIR="${CHROMIUM_DATA_DIR}"

# 直接以 root 运行 Chromium (HOME=/home/user)
exec /usr/bin/chromium \
    --user-data-dir="${CHROMIUM_DATA_DIR}" \
    --no-sandbox \
    --disable-dev-shm-usage \
    --remote-debugging-port=9222 \
    --remote-debugging-address=0.0.0.0 \
    --no-first-run \
    --no-default-browser-check \
    --password-store=basic \
    --use-mock-keychain \
    "$@"

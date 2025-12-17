#!/bin/bash
# MCP 专用 Chromium 包装器
# 以 user 用户身份运行 Chromium，确保能连接到 fcitx5 输入法的 D-Bus 会话

# 读取 D-Bus 会话地址
if [ -f /tmp/dbus-session-env ]; then
    source /tmp/dbus-session-env
fi

# 读取 Chromium 环境变量
if [ -f /etc/profile.d/chromium-env.sh ]; then
    source /etc/profile.d/chromium-env.sh
fi

# 使用共享的数据目录（与手动打开的 Chromium 相同）
CHROMIUM_DATA_DIR="${CHROMIUM_USER_DATA_DIR:-/app/computer-project-workspace/.chromium-data-shared}"

# 以 user 用户身份运行 Chromium
# 所有参数通过 $@ 传递（来自 chrome-devtools-mcp）
# 关键：使用与手动 Chromium 相同的方式，通过 su - user 运行
exec su - user -c "
    export DISPLAY=:0
    export DBUS_SESSION_BUS_ADDRESS='${DBUS_SESSION_BUS_ADDRESS}'
    export GTK_IM_MODULE=fcitx5
    export QT_IM_MODULE=fcitx5
    export XMODIFIERS=@im=fcitx5
    export INPUT_METHOD=fcitx5
    export SDL_IM_MODULE=fcitx5
    export GLFW_IM_MODULE=fcitx5
    export LANG=C.UTF-8
    export LC_ALL=C.UTF-8
    exec /usr/bin/chromium --user-data-dir='${CHROMIUM_DATA_DIR}' \$@
" -- "$@"

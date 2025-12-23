#!/bin/bash
# Chromium Browser Launcher with fcitx input method support
# Runs as root with HOME=/home/user

# 加载环境配置
[ -f /etc/profile.d/fcitx5-env.sh ] && source /etc/profile.d/fcitx5-env.sh
[ -f /tmp/dbus-session-env ] && source /tmp/dbus-session-env
[ -f /etc/profile.d/chromium-env.sh ] && source /etc/profile.d/chromium-env.sh
[ -f /etc/profile.d/ime-env.sh ] && source /etc/profile.d/ime-env.sh

export DISPLAY=:0
export HOME=/home/user

# 使用 @im=fcitx 保持一致性
export GTK_IM_MODULE=fcitx
export QT_IM_MODULE=fcitx
export XMODIFIERS=@im=fcitx
export INPUT_METHOD=fcitx

CHROMIUM_DATA_DIR="${CHROMIUM_USER_DATA_DIR:-/home/user/.config/chromium}"

# 调用原始的 chromium wrapper（已备份为 chromium-bin）
exec /usr/bin/chromium-bin \
    --user-data-dir="$CHROMIUM_DATA_DIR" \
    --no-sandbox \
    --disable-dev-shm-usage \
    --remote-debugging-port=9222 \
    --remote-debugging-address=0.0.0.0 \
    --no-first-run \
    --no-default-browser-check \
    --password-store=basic \
    --use-mock-keychain \
    --disable-session-crashed-bubble \
    --disable-infobars \
    --no-process-singleton-dialog \
    --force-color-profile=srgb "$@"

#!/bin/bash
# Chromium 中文输入法修复工具（fcitx5 方案）
# 使用方法：在 VNC 终端中运行 fix-ime

echo "=========================================="
echo "  Chromium 中文输入法修复工具"
echo "=========================================="
echo ""

# ========== 加载全局输入法环境 ==========
if [ -f /etc/profile.d/ime-env.sh ]; then
    source /etc/profile.d/ime-env.sh
    echo "✓ 已加载全局输入法环境配置"
else
    echo "⚠ 未找到全局配置，手动设置环境变量"
    # 设置环境变量
    export DISPLAY=:0
    export XDG_RUNTIME_DIR=/run/user/1000
    export LANG=C.UTF-8
    export LC_ALL=C.UTF-8
    export GTK_IM_MODULE=fcitx
    export QT_IM_MODULE=fcitx
    export XMODIFIERS=@im=fcitx
    export INPUT_METHOD=fcitx

    # 导入 D-Bus 会话地址
    if [ -f /tmp/dbus-session-env ]; then
        source /tmp/dbus-session-env
    fi
fi

echo "当前环境变量："
echo "  DISPLAY=$DISPLAY"
echo "  GTK_IM_MODULE=$GTK_IM_MODULE"
echo "  XMODIFIERS=$XMODIFIERS"
echo "  LANG=$LANG"
echo "  DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS"

echo ""
echo "第 1 步：检查 fcitx5 状态..."
if pgrep -x fcitx5 > /dev/null; then
    echo "  ✓ fcitx5 已在运行 (PID: $(pgrep -x fcitx5 | head -1))"
else
    echo "  ⚠ fcitx5 未运行，正在启动..."
    fcitx5 -d --replace >/tmp/fcitx5.log 2>&1 &
    sleep 2

    if pgrep -x fcitx5 > /dev/null; then
        echo "  ✓ fcitx5 启动成功"
    else
        echo "  ✗ fcitx5 启动失败，查看 /tmp/fcitx5.log"
    fi
fi

echo ""
echo "第 2 步：停止所有 Chromium 进程..."
killall -9 chromium chrome 2>/dev/null || true
sleep 2
echo "✓ Chromium 进程已清理"

echo ""
echo "第 3 步：使用正确的环境变量启动 Chromium..."
echo "提示：Chromium 将继承以下输入法环境："
echo "  - GTK_IM_MODULE=fcitx"
echo "  - XMODIFIERS=@im=fcitx"
echo "  - LANG=C.UTF-8"
echo ""

/usr/bin/chromium \
  --user-data-dir=/home/user/chromium-data \
  --no-sandbox \
  --disable-dev-shm-usage \
  --remote-debugging-port=9222 \
  --remote-debugging-address=0.0.0.0 \
  --no-first-run \
  --no-default-browser-check \
  --password-store=basic \
  --use-mock-keychain \
  >/tmp/chromium-ime.log 2>&1 &

sleep 3

if pgrep chromium > /dev/null; then
    echo "  ✓ Chromium 启动成功 (PID: $(pgrep chromium | head -1))"

    # 验证 Chromium 是否继承了正确的环境变量
    CHROME_PID=$(pgrep chromium | head -1)
    echo ""
    echo "验证 Chromium 环境变量："
    cat /proc/$CHROME_PID/environ | tr '\0' '\n' | grep -E "(GTK_IM_MODULE|XMODIFIERS|LANG)" || echo "  ⚠ 警告：未找到输入法环境变量"
else
    echo "  ✗ Chromium 启动失败，查看 /tmp/chromium-ime.log"
    exit 1
fi

echo ""
echo "=========================================="
echo "  ✅ 修复完成！"
echo "=========================================="
echo ""
echo "现在请测试："
echo "  1. 在 Chromium 中访问 baidu.com 或任何网页"
echo "  2. 点击输入框（搜索框、文本框等）"
echo "  3. 按 Ctrl+Space 切换输入法"
echo "  4. 输入拼音（如 nihao）应显示候选词"
echo ""
echo "如果仍无法输入中文，请尝试："
echo "  - 重启容器使环境变量全局生效"
echo "  - 或者使用 'source /etc/profile.d/ime-env.sh' 再次运行本脚本"
echo "=========================================="

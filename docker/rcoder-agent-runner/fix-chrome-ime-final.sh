#!/bin/bash
# Chromium 中文输入法修复工具（纯 fcitx5 方案）
# 使用方法：在 VNC 终端中运行 fix-ime

echo "=========================================="
echo "  Chromium 中文输入法修复工具"
echo "=========================================="
echo ""

# 设置环境变量
export DISPLAY=:0
export XDG_RUNTIME_DIR=/run/user/1000

# 导入 D-Bus 会话地址
if [ -f /tmp/dbus-session-env ]; then
    source /tmp/dbus-session-env
    echo "✓ D-Bus 会话地址: $DBUS_SESSION_BUS_ADDRESS"
else
    echo "⚠ 未找到 D-Bus 会话配置"
fi

echo ""
echo "第 1 步：停止所有相关进程..."
killall -9 chromium chrome 2>/dev/null || true
killall -9 fcitx5 2>/dev/null || true
sleep 2
echo "✓ 进程已清理"

echo ""
echo "第 2 步：启动 fcitx5..."
fcitx5 -d --replace >/tmp/fcitx5.log 2>&1 &
sleep 2

if pgrep -x fcitx5 > /dev/null; then
    echo "  ✓ fcitx5 启动成功"
else
    echo "  ✗ fcitx5 启动失败，查看 /tmp/fcitx5.log"
fi

echo ""
echo "第 3 步：设置输入法环境变量..."
export GTK_IM_MODULE=fcitx5
export QT_IM_MODULE=fcitx5
export XMODIFIERS=@im=fcitx5
export INPUT_METHOD=fcitx5
echo "  ✓ 环境变量已设置"

echo ""
echo "第 4 步：启动 Chromium..."
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
echo "  1. 在 Chromium 中访问 baidu.com"
echo "  2. 点击搜索框"
echo "  3. 按 Ctrl+Space 切换输入法"
echo "  4. 输入拼音（如 nihao）"
echo "=========================================="

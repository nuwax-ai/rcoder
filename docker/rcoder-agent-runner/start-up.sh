#!/bin/bash

# ============================================================================
# 🎯 容器日志持久化设置
# 将日志输出到挂载的持久化目录，即使容器崩溃也能保留日志
# ============================================================================
CONTAINER_LOGS_DIR="${CONTAINER_LOGS_DIR:-/app/container-logs}"
CONTAINER_LOG_NAME="${CONTAINER_LOG_NAME:-unknown}"

# 检查日志目录是否可用（由主容器挂载）
if [ -d "$CONTAINER_LOGS_DIR" ] && [ -w "$CONTAINER_LOGS_DIR" ]; then
    # 创建日志文件
    STARTUP_LOG="$CONTAINER_LOGS_DIR/startup.log"
    ERROR_LOG="$CONTAINER_LOGS_DIR/error.log"
    AGENT_LOG="$CONTAINER_LOGS_DIR/agent.log"

    # 记录容器启动信息
    echo "============================================" >> "$STARTUP_LOG"
    echo "Container startup at $(date '+%Y-%m-%d %H:%M:%S %Z')" >> "$STARTUP_LOG"
    echo "Container hostname: $(hostname)" >> "$STARTUP_LOG"
    echo "Log directory name: $CONTAINER_LOG_NAME" >> "$STARTUP_LOG"
    echo "USER_ID: ${USER_ID:-not set}" >> "$STARTUP_LOG"
    echo "PROJECT_ID: ${PROJECT_ID:-not set}" >> "$STARTUP_LOG"
    echo "============================================" >> "$STARTUP_LOG"

    # 将标准输出和错误输出同时发送到终端和日志文件
    # stdout -> tee -> STARTUP_LOG
    # stderr -> tee -> ERROR_LOG
    exec > >(tee -a "$STARTUP_LOG") 2> >(tee -a "$ERROR_LOG" >&2)

    echo "📁 容器日志持久化已启用: $CONTAINER_LOGS_DIR"
    echo "   - startup.log: 启动日志"
    echo "   - error.log: 错误日志"
    echo "   - agent.log: Agent 运行日志 (如有)"

    # 导出日志路径供其他进程使用
    export CONTAINER_STARTUP_LOG="$STARTUP_LOG"
    export CONTAINER_ERROR_LOG="$ERROR_LOG"
    export CONTAINER_AGENT_LOG="$AGENT_LOG"
else
    echo "⚠️  容器日志目录不可用，使用默认输出"
fi
function start_vnc_services() {
	echo "Starting VNC services..."

	# 等待X11服务完全启动
	counter=0
	while ! su - user -c "DISPLAY=:0 xdpyinfo" >/dev/null 2>&1; do
		sleep 0.5
		let counter++
		if ((counter > 30)); then
			echo "X11 not ready, skipping VNC startup"
			return 1
		fi
	done

	echo "X11 is ready, starting VNC..."

	# 停止可能存在的VNC服务
	su - user -c "pkill x11vnc || true" 2>/dev/null

	# 等待进程完全停止
	sleep 2

	# 启动x11vnc服务器 (后台运行)
	su - user -c "
		export DISPLAY=:0
		nohup x11vnc -bg -display :0 -forever -wait 50 -shared -rfbport 5900 -nopw 2>/tmp/x11vnc_stderr.log >/dev/null &
	" &

	# 等待x11vnc启动
	sleep 3

	# 启动noVNC代理 (后台运行)
	su - user -c "
		export DISPLAY=:0
		cd /opt/noVNC/utils
		nohup ./novnc_proxy --vnc localhost:5900 --listen 6080 --web /opt/noVNC > /tmp/novnc.log 2>&1 &
	" &

	# 等待noVNC启动
	sleep 3

	# 检查VNC服务状态
	vnc_running=false
	novnc_running=false

	# 检查x11vnc进程
	if su - user -c "pgrep -x x11vnc" >/dev/null 2>&1; then
		vnc_running=true
		echo "✓ x11vnc server started on port 5900"
	else
		echo "✗ x11vnc server failed to start"
		echo "Error log:"
		su - user -c "cat /tmp/x11vnc_stderr.log 2>/dev/null || echo 'No error log found'"
	fi

	# 检查noVNC端口
	if netstat -tuln 2>/dev/null | grep -q ":6080 "; then
		novnc_running=true
		echo "✓ noVNC proxy started on port 6080"
		echo "  VNC URL: http://localhost:6080/vnc.html?autoconnect=true&resize=scale"
	else
		echo "✗ noVNC proxy failed to start"
		echo "Error log:"
		su - user -c "cat /tmp/novnc.log 2>/dev/null || echo 'No error log found'"
	fi

	if [ "$vnc_running" = true ] && [ "$novnc_running" = true ]; then
		echo "✓ VNC services started successfully!"
		return 0
	else
		echo "✗ VNC services failed to start properly"
		return 1
	fi
}

function start_display_and_desktop() {
	echo "Starting X11 display server and XFCE4 desktop..."

	# 清理可能存在的X11锁文件和进程
	rm -f /tmp/.X0-lock
	rm -rf /tmp/.X11-unix/X0
	pkill -f "Xvfb :0" || true
	pkill -f "xfce4-session" || true
	pkill -f "dbus-daemon" || true
	pkill -f "fcitx5" || true

	# ========== 关键修复：清理 Chromium 进程和锁文件 ==========
	echo "🧹 Cleaning up stale Chromium processes and lock files..."

	# 1. 强制终止所有遗留的 Chromium 进程
	pkill -9 -f "chromium" || true
	pkill -9 -f "chrome" || true

	# 2. 设置持久化的 Chromium 数据目录路径（基于 USER_ID）
	# 优先级：环境变量 > 默认持久化路径 > 回退到临时目录
	if [ -n "${USER_ID}" ] && [ -d "/app/computer-project-workspace/${USER_ID}" ]; then
		CHROMIUM_USER_DATA_DIR="/app/computer-project-workspace/${USER_ID}/.chromium-data"
		echo "✅ 使用持久化 Chromium 数据目录: $CHROMIUM_USER_DATA_DIR (user_id=${USER_ID})"
	elif [ -d "/app/computer-project-workspace" ]; then
		# 回退：如果没有 USER_ID，使用共享目录（不推荐，但保证兼容性）
		CHROMIUM_USER_DATA_DIR="/app/computer-project-workspace/.chromium-data-shared"
		echo "⚠️  USER_ID 未设置，使用共享 Chromium 数据目录: $CHROMIUM_USER_DATA_DIR"
	else
		# 最终回退：使用容器内临时目录
		CHROMIUM_USER_DATA_DIR="/home/user/chromium-data"
		echo "⚠️  持久化目录不可用，使用临时 Chromium 数据目录: $CHROMIUM_USER_DATA_DIR"
	fi

	# 3. 创建 Chromium 数据目录（如果不存在）
	mkdir -p "$CHROMIUM_USER_DATA_DIR"
	chown -R user:user "$CHROMIUM_USER_DATA_DIR"
	chmod -R 777 "$CHROMIUM_USER_DATA_DIR"

	# 4. 导出环境变量供后续进程使用
	export CHROMIUM_USER_DATA_DIR
	echo "export CHROMIUM_USER_DATA_DIR='${CHROMIUM_USER_DATA_DIR}'" >> /etc/environment
	echo "export CHROMIUM_USER_DATA_DIR='${CHROMIUM_USER_DATA_DIR}'" >> /etc/profile.d/chromium-env.sh

	# 5. 清理 Chromium profile 锁文件（SingletonLock）
	if [ -d "$CHROMIUM_USER_DATA_DIR" ]; then
		# 删除 SingletonLock 文件
		rm -f "$CHROMIUM_USER_DATA_DIR/SingletonLock" || true
		rm -f "$CHROMIUM_USER_DATA_DIR/SingletonSocket" || true
		rm -f "$CHROMIUM_USER_DATA_DIR/SingletonCookie" || true

		# 清理崩溃状态文件
		rm -rf "$CHROMIUM_USER_DATA_DIR/Crash Reports/pending/"* || true
		rm -f "$CHROMIUM_USER_DATA_DIR/.org.chromium.Chromium."* || true

		# 删除可能的临时锁文件
		find "$CHROMIUM_USER_DATA_DIR" -name "*.lock" -type f -delete 2>/dev/null || true
		find "$CHROMIUM_USER_DATA_DIR" -name "lockfile" -type f -delete 2>/dev/null || true

		echo "✅ Chromium lock files cleaned from: $CHROMIUM_USER_DATA_DIR"
	fi

	# 6. 清理 /tmp 中的 Chromium 临时文件
	rm -rf /tmp/.org.chromium.Chromium.* || true
	rm -rf /tmp/chrome_* || true

	# 7. 清理 /dev/shm 中的 Chromium 共享内存
	rm -rf /dev/shm/.org.chromium.Chromium.* || true

	echo "✅ Chromium cleanup completed (data dir: $CHROMIUM_USER_DATA_DIR)"

	# 创建用户运行时目录并设置权限
	USER_ID=$(id -u user)
	mkdir -p /run/user/${USER_ID}
	chmod 700 /run/user/${USER_ID}
	chown user:user /run/user/${USER_ID}

	# ========== 关键修复：设置 UTF-8 locale ==========
	# 确保 locale 是 UTF-8，否则中文输入会失败
	export LANG=C.UTF-8
	export LC_ALL=C.UTF-8
	export LC_CTYPE=C.UTF-8

	# 启动 D-Bus 会话 (以 user 用户启动，并保存地址)
	echo "Starting D-Bus session as user..."
	su - user -c "dbus-launch --sh-syntax > /tmp/dbus-session-env"
	sleep 2

	# 导出 D-Bus 会话地址供后续使用
	DBUS_ADDR=""
	if [ -f /tmp/dbus-session-env ]; then
		source /tmp/dbus-session-env
		DBUS_ADDR="$DBUS_SESSION_BUS_ADDRESS"
		echo "D-Bus session: $DBUS_ADDR"

		# ========== 关键修复：将 D-Bus 地址导出到全局环境 ==========
		# 将 D-Bus 会话地址写入 /etc/environment，确保所有后续进程都能访问
		echo "DBUS_SESSION_BUS_ADDRESS=\"${DBUS_ADDR}\"" >> /etc/environment
		# 同时导出到当前 shell 环境
		export DBUS_SESSION_BUS_ADDRESS="${DBUS_ADDR}"
		echo "✓ D-Bus address exported to global environment"

		# ========== 关键修复：允许 root 访问 user 的 D-Bus socket ==========
		# 修改 D-Bus socket 文件权限，允许 root 用户连接（用于 MCP chromium 中文输入）
		chmod 777 /tmp/dbus-* 2>/dev/null || true
		echo "✓ D-Bus socket permissions updated for root access"
	fi

	# 启动 D-Bus 系统总线
	echo "Starting D-Bus system bus..."
	mkdir -p /var/run/dbus
	dbus-daemon --system --fork
	sleep 1

	# 启动 PolicyKit 守护进程（配置为不需要认证）
	echo "Starting PolicyKit daemon..."
	/usr/lib/policykit-1/polkitd --no-debug >/var/log/polkitd.log 2>&1 &
	sleep 2

	# 以user用户启动Xvfb
	su - user -c "Xvfb :0 -ac -screen 0 1280x800x16 -retro -dpi 96 -nolisten tcp -nolisten unix >/dev/null 2>&1" &

	# 等待Xvfb启动
	counter=0
	while ! su - user -c "DISPLAY=:0 xdpyinfo" >/dev/null 2>&1; do
		sleep 0.1
		let counter++
		if ((counter > 100)); then
			echo "Failed to start Xvfb"
			return 1
		fi
	done

	# ========== 关键修复：手动启动 fcitx5，确保环境变量正确 ==========
	# 不再依赖 XFCE autostart，直接用正确的环境变量启动
	echo "Starting fcitx5 input method..."
	su - user -c "
		export DISPLAY=:0
		export DBUS_SESSION_BUS_ADDRESS='${DBUS_ADDR}'
		export LANG=C.UTF-8
		export LC_ALL=C.UTF-8
		export LC_CTYPE=C.UTF-8
		# 使用 @im=fcitx 与 GTK immodule cache 兼容（cache 中注册的名称是 fcitx）
		export GTK_IM_MODULE=fcitx
		export QT_IM_MODULE=fcitx
		export XMODIFIERS=@im=fcitx
		export INPUT_METHOD=fcitx
		fcitx5 -d --replace >/tmp/fcitx5-startup.log 2>&1
	" &
	sleep 3

	# 验证 fcitx5 启动成功
	if pgrep -x fcitx5 >/dev/null 2>&1; then
		echo "✓ fcitx5 started successfully"
	else
		echo "✗ fcitx5 failed to start, check /tmp/fcitx5-startup.log"
	fi

	# 以user用户启动XFCE4会话
	# 注意：使用 @im=fcitx 与系统 immodule 兼容
	su - user -c "
		export DISPLAY=:0
		export XDG_CURRENT_DESKTOP=XFCE
		export XDG_SESSION_DESKTOP=xfce
		export XDG_RUNTIME_DIR=/run/user/${USER_ID}
		export GNOME_KEYRING_CONTROL=/run/user/${USER_ID}/keyring
		export GTK_MODULES=gnome-keyring-pkcs11

		# ========== 关键修复：使用双引号确保变量展开 ==========
		export DBUS_SESSION_BUS_ADDRESS=\"${DBUS_ADDR}\"

		# ========== 关键修复：设置 UTF-8 locale ==========
		export LANG=C.UTF-8
		export LC_ALL=C.UTF-8
		export LC_CTYPE=C.UTF-8

		# ========== 关键修复：使用 @im=fcitx 与 GTK immodule 兼容 ==========
		# fcitx5 的 GTK immodule 在系统 cache 中注册的名称是 'fcitx'，不是 'fcitx5'
		export GTK_IM_MODULE=fcitx
		export QT_IM_MODULE=fcitx
		export XMODIFIERS=@im=fcitx
		export INPUT_METHOD=fcitx
		export SDL_IM_MODULE=fcitx
		export GLFW_IM_MODULE=ibus

		echo \"Environment variables set:\"
		echo \"  GTK_IM_MODULE=\$GTK_IM_MODULE\"
		echo \"  XMODIFIERS=\$XMODIFIERS\"
		echo \"  DBUS_SESSION_BUS_ADDRESS=\$DBUS_SESSION_BUS_ADDRESS\"
		echo \"  LANG=\$LANG\"

		# 启动 gnome-keyring-daemon
		gnome-keyring-daemon --start --components=secrets,ssh,pkcs11 >/dev/null 2>&1 &

		# 启动 PolicyKit 认证代理
		/usr/lib/policykit-1-gnome/polkit-gnome-authentication-agent-1 >/var/log/polkit-agent.log 2>&1 &

		# 等待守护进程启动
		sleep 2

		# fcitx5 已经在前面手动启动，不再依赖 XFCE autostart
		echo 'Fcitx5 already started manually'

		# 使用 env 明确传递环境变量启动 XFCE4
		# 不再使用 --exit-with-session，避免 xfce4-session 创建新的 D-Bus session
		exec env \
			DISPLAY=:0 \
			XDG_CURRENT_DESKTOP=XFCE \
			XDG_SESSION_DESKTOP=xfce \
			XDG_RUNTIME_DIR=/run/user/${USER_ID} \
			DBUS_SESSION_BUS_ADDRESS=\"${DBUS_ADDR}\" \
			LANG=C.UTF-8 \
			LC_ALL=C.UTF-8 \
			LC_CTYPE=C.UTF-8 \
			GTK_IM_MODULE=fcitx \
			QT_IM_MODULE=fcitx \
			XMODIFIERS=@im=fcitx \
			INPUT_METHOD=fcitx \
			xfce4-session
	" &

	echo "X11 display and XFCE4 desktop started successfully"
}

function check_vnc_health() {
    # 检查VNC服务健康状态
    if [ "$VNC_AUTO_START" = "true" ]; then
        # 检查x11vnc进程
        if ! su - user -c "pgrep -x x11vnc" >/dev/null 2>&1; then
            echo "⚠️  x11vnc process not running, attempting restart..."
            return 1
        fi

        # 检查noVNC端口
        if ! netstat -tuln 2>/dev/null | grep -q ":6080 "; then
            echo "⚠️  noVNC proxy not listening on port 6080, attempting restart..."
            return 1
        fi

        echo "✓ VNC services are healthy"
        return 0
    fi
    return 0
}

# Jupyter server function removed

echo "Starting Code Interpreter server..."

# 设置VNC自动启动标志
export VNC_AUTO_START=true

# 首先启动显示服务和桌面环境
start_display_and_desktop &

# 设置全局DISPLAY环境变量
export DISPLAY=:0
echo "DISPLAY=:0" >> /etc/environment

# 启动envd和其他服务
/bin/bash -l -c "DISPLAY=:0 /usr/bin/envd" &

# Jupyter services removed

# 启动 VNC 服务（在后台运行，等待X11就绪）
echo "Starting VNC services in background..."
echo "VNC will be available at: http://localhost:6080/vnc.html?autoconnect=true&resize=scale"
(
    # 等待X11服务就绪
    counter=0
    while ! su - user -c "DISPLAY=:0 xdpyinfo" >/dev/null 2>&1; do
        sleep 1
        let counter++
        if ((counter > 60)); then
            echo "Timeout waiting for X11, VNC services will not start"
            exit 1
        fi
    done

    echo "X11 is ready, starting VNC services..."
    start_vnc_services

    echo "✓ VNC services started successfully!"
    echo "✓ VNC URL: http://localhost:6080/vnc.html?autoconnect=true&resize=scale"
    echo "✓ Direct VNC port: 5900"

    # VNC服务监控循环
    while true; do
        sleep 30
        if ! check_vnc_health; then
            echo "VNC服务异常，正在重启..."
            # 停止现有服务
            su - user -c "pkill x11vnc || true"
            su - user -c "pkill -f novnc_proxy || true"
            sleep 2
            # 重新启动VNC服务
            start_vnc_services
        fi
    done
) &

# 启动 agent_runner 服务，支持从环境变量读取端口
echo "Starting agent_runner service on port ${PORT:-8086}..."

# ========== 关键修复：确保 agent_runner 及其子进程继承输入法环境 ==========
# 从 /tmp/dbus-session-env 加载 D-Bus 地址
if [ -f /tmp/dbus-session-env ]; then
    source /tmp/dbus-session-env
    export DBUS_SESSION_BUS_ADDRESS
    echo "✓ agent_runner will use D-Bus: $DBUS_SESSION_BUS_ADDRESS"

    # ========== 新增：将 D-Bus 地址写入全局环境 ==========
    # 确保所有子进程（包括 chrome-devtools-mcp 启动的 Chromium）都能访问
    echo "export DBUS_SESSION_BUS_ADDRESS='${DBUS_SESSION_BUS_ADDRESS}'" >> /etc/profile.d/ime-env.sh
fi

# ========== 新增：创建全局输入法环境配置文件 ==========
# 所有进程（包括 agent_runner、chrome-devtools-mcp、Chromium）都会继承这些环境变量
cat > /etc/profile.d/ime-env.sh <<'EOF'
# Fcitx5 中文输入法环境变量
export DISPLAY=:0
export GTK_IM_MODULE=fcitx
export QT_IM_MODULE=fcitx
export XMODIFIERS=@im=fcitx
export INPUT_METHOD=fcitx
export LANG=C.UTF-8
export LC_ALL=C.UTF-8
export LC_CTYPE=C.UTF-8
EOF

# 立即加载环境配置
source /etc/profile.d/ime-env.sh

# 确保输入法环境变量被导出到当前 shell
export DISPLAY=:0
export GTK_IM_MODULE=fcitx
export QT_IM_MODULE=fcitx
export XMODIFIERS=@im=fcitx
export INPUT_METHOD=fcitx
export LANG=C.UTF-8
export LC_ALL=C.UTF-8
export LC_CTYPE=C.UTF-8

echo "✓ Input method environment variables exported globally (/etc/profile.d/ime-env.sh)"

# ========== 新增：将环境变量写入 /etc/environment（systemd 使用）==========
# 确保通过 systemd 启动的服务也能继承
cat >> /etc/environment <<EOF
DISPLAY=:0
GTK_IM_MODULE=fcitx
QT_IM_MODULE=fcitx
XMODIFIERS=@im=fcitx
INPUT_METHOD=fcitx
LANG=C.UTF-8
LC_ALL=C.UTF-8
LC_CTYPE=C.UTF-8
EOF

echo "✓ Input method environment variables written to /etc/environment"

# agent_runner 在前台运行，作为主进程保持容器运行
exec /usr/local/bin/agent_runner -p ${PORT:-8086}

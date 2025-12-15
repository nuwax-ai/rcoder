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

	# 等待x11vnc启computer-agent-runner-user_123动
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

	# 创建用户运行时目录并设置权限
	USER_ID=$(id -u user)
	mkdir -p /run/user/${USER_ID}
	chmod 700 /run/user/${USER_ID}
	chown user:user /run/user/${USER_ID}

	# 启动 D-Bus 会话 (以 user 用户启动，并保存地址)
	echo "Starting D-Bus session as user..."
	su - user -c "dbus-launch --sh-syntax > /tmp/dbus-session-env"
	sleep 2

	# 导出 D-Bus 会话地址供后续使用
	if [ -f /tmp/dbus-session-env ]; then
		source /tmp/dbus-session-env
		echo "D-Bus session: $DBUS_SESSION_BUS_ADDRESS"
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

	# 导入 D-Bus 会话地址
	if [ -f /tmp/dbus-session-env ]; then
		source /tmp/dbus-session-env
		echo "D-Bus session loaded: $DBUS_SESSION_BUS_ADDRESS"
	fi

	# 以user用户启动XFCE4会话和所有必要的守护进程
	su - user -c "
		export DISPLAY=:0
		export XDG_CURRENT_DESKTOP=XFCE
		export XDG_SESSION_DESKTOP=xfce
		export XDG_RUNTIME_DIR=/run/user/${USER_ID}
		export GNOME_KEYRING_CONTROL=/run/user/${USER_ID}/keyring
		export GTK_MODULES=gnome-keyring-pkcs11

		# 导入 D-Bus 会话地址
		export DBUS_SESSION_BUS_ADDRESS='$DBUS_SESSION_BUS_ADDRESS'

		# 设置输入法环境变量（纯 fcitx5）
		export GTK_IM_MODULE=fcitx5
		export QT_IM_MODULE=fcitx5
		export XMODIFIERS=@im=fcitx5
		export INPUT_METHOD=fcitx5
		export SDL_IM_MODULE=fcitx5
		export GLFW_IM_MODULE=fcitx5

		echo \"Environment variables set:\"
		echo \"  GTK_IM_MODULE=\$GTK_IM_MODULE\"
		echo \"  DBUS_SESSION_BUS_ADDRESS=\$DBUS_SESSION_BUS_ADDRESS\"

		# 启动 gnome-keyring-daemon
		gnome-keyring-daemon --start --components=secrets,ssh,pkcs11 >/dev/null 2>&1 &

		# 启动 PolicyKit 认证代理
		/usr/lib/policykit-1-gnome/polkit-gnome-authentication-agent-1 >/var/log/polkit-agent.log 2>&1 &

		# 等待守护进程启动
		sleep 2

		# 输入法框架（fcitx5）将由 XFCE 自启动项自动启动
		echo 'Fcitx5 will be started by XFCE autostart'

		# 使用 env 明确传递环境变量启动 XFCE4
		exec env \
			DISPLAY=:0 \
			XDG_CURRENT_DESKTOP=XFCE \
			XDG_SESSION_DESKTOP=xfce \
			XDG_RUNTIME_DIR=/run/user/${USER_ID} \
			DBUS_SESSION_BUS_ADDRESS=\"\$DBUS_SESSION_BUS_ADDRESS\" \
			GTK_IM_MODULE=ibus \
			QT_IM_MODULE=ibus \
			XMODIFIERS=@im=ibus \
			INPUT_METHOD=fcitx5 \
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
# agent_runner 在前台运行，作为主进程保持容器运行
exec /usr/local/bin/agent_runner -p ${PORT:-8086}

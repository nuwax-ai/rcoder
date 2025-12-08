#!/bin/bash

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

function start_jupyter_server() {
	counter=0
	response=$(curl -s -o /dev/null -w "%{http_code}" "http://localhost:8888/api/status")
	while [[ ${response} -ne 200 ]]; do
		let counter++
		if ((counter % 20 == 0)); then
			echo "Waiting for Jupyter Server to start..."
			sleep 0.1
		fi

		response=$(curl -s -o /dev/null -w "%{http_code}" "http://localhost:8888/api/status")
	done

	cd /root/.server/
	# 在后台启动 uvicorn 服务
	/root/.server/.venv/bin/uvicorn main:app --host 0.0.0.0 --port 49999 --workers 1 --no-access-log --no-use-colors --timeout-keep-alive 640 &
}

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

# 启动 Jupyter 服务器（后台等待）
start_jupyter_server &

# 启动 Jupyter 主服务（后台运行）
echo "Starting Jupyter server..."
MATPLOTLIBRC=/root/.config/matplotlib/.matplotlibrc jupyter server --IdentityProvider.token="" &

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

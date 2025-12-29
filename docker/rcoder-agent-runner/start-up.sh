#!/bin/bash

# ============================================================================
# 📝 带时间戳的日志函数
# 所有日志输出都会自动添加 UTC 时间前缀，格式与 agent_runner 一致
# 格式：YYYY-MM-DDTHH:MM:SS.ffffffZ（ISO 8601 UTC）
# 注意：日志函数必须在脚本最开头定义，因为后面的代码会立即使用它们
# ============================================================================
function log() {
    echo "$(date -u '+%Y-%m-%dT%H:%M:%S.%6NZ')  INFO $*"
}

function log_info() {
    echo "$(date -u '+%Y-%m-%dT%H:%M:%S.%6NZ')  INFO ℹ️  $*"
}

function log_success() {
    echo "$(date -u '+%Y-%m-%dT%H:%M:%S.%6NZ')  INFO ✓ $*"
}

function log_warn() {
    echo "$(date -u '+%Y-%m-%dT%H:%M:%S.%6NZ')  WARN ⚠️  $*"
}

function log_error() {
    echo "$(date -u '+%Y-%m-%dT%H:%M:%S.%6NZ') ERROR ❌ $*"
}

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

    log "容器日志持久化已启用: $CONTAINER_LOGS_DIR"
    echo "   - startup.log: 启动日志"
    echo "   - error.log: 错误日志"
    echo "   - agent.log: Agent 运行日志 (如有)"

    # 导出日志路径供其他进程使用
    export CONTAINER_STARTUP_LOG="$STARTUP_LOG"
    export CONTAINER_ERROR_LOG="$ERROR_LOG"
    export CONTAINER_AGENT_LOG="$AGENT_LOG"
else
    log_warn " 容器日志目录不可用，使用默认输出"
fi


# ============================================================================
# 🎯 动态时区设置（支持通过 TZ 环境变量自定义时区）
# 默认时区为 Asia/Shanghai（在 Dockerfile 中配置）
# 启动时如果检测到 TZ 环境变量，则更新系统时区
# 使用方式: docker run -e TZ=America/New_York your-image
# ============================================================================
function initialize_timezone() {
    # 如果 TZ 环境变量未设置或为空，保持默认时区（Asia/Shanghai）
    if [ -z "$TZ" ]; then
        log "Using default timezone: Asia/Shanghai"
        return 0
    fi

    log "Setting timezone to: $TZ"

    # 检查时区文件是否存在
    if [ -f "/usr/share/zoneinfo/$TZ" ]; then
        # 更新 /etc/localtime 软链接
        ln -sf "/usr/share/zoneinfo/$TZ" /etc/localtime
        # 更新 /etc/timezone 文件
        echo "$TZ" > /etc/timezone
        log_success "Timezone set to $TZ"
    else
        log_warn " Invalid timezone: $TZ (file /usr/share/zoneinfo/$TZ not found)"
        echo "   Available timezones can be found in /usr/share/zoneinfo/"
        echo "   Keeping default timezone: Asia/Shanghai"
    fi
}

# ============================================================================
# 🔧 通用等待 Helper 函数（用于替代固定 sleep 阻塞）
# 使用轮询机制，可提前返回，大幅减少启动时间
# 注意：使用纯 bash 整数运算，无需依赖 bc 命令
# ============================================================================

# 等待进程启动，最长等待 $2 秒（默认 10 秒）
# 用法: wait_for_process "process_name" [timeout_seconds]
function wait_for_process() {
    local process_name="$1"
    local timeout="${2:-10}"
    local interval_ms=200  # 200ms 间隔
    local max_iterations=$((timeout * 1000 / interval_ms))
    local i=0

    while ! pgrep -x "$process_name" >/dev/null 2>&1; do
        sleep 0.2
        i=$((i + 1))
        if [ $i -ge $max_iterations ]; then
            return 1
        fi
    done
    return 0
}

# 等待进程启动（支持模式匹配），最长等待 $2 秒（默认 10 秒）
# 用法: wait_for_process_pattern "pattern" [timeout_seconds]
function wait_for_process_pattern() {
    local pattern="$1"
    local timeout="${2:-10}"
    local interval_ms=200
    local max_iterations=$((timeout * 1000 / interval_ms))
    local i=0

    while ! pgrep -f "$pattern" >/dev/null 2>&1; do
        sleep 0.2
        i=$((i + 1))
        if [ $i -ge $max_iterations ]; then
            return 1
        fi
    done
    return 0
}

# 等待端口可用，最长等待 $3 秒（默认 10 秒）
# 用法: wait_for_port host port [timeout_seconds]
function wait_for_port() {
    local host="$1"
    local port="$2"
    local timeout="${3:-10}"
    local interval_ms=300  # 300ms 间隔
    local max_iterations=$((timeout * 1000 / interval_ms))
    local i=0

    while ! nc -z "$host" "$port" 2>/dev/null; do
        sleep 0.3
        i=$((i + 1))
        if [ $i -ge $max_iterations ]; then
            return 1
        fi
    done
    return 0
}

# 等待文件存在，最长等待 $2 秒（默认 5 秒）
# 用法: wait_for_file "filepath" [timeout_seconds]
function wait_for_file() {
    local filepath="$1"
    local timeout="${2:-5}"
    local interval_ms=200
    local max_iterations=$((timeout * 1000 / interval_ms))
    local i=0

    while [ ! -f "$filepath" ]; do
        sleep 0.2
        i=$((i + 1))
        if [ $i -ge $max_iterations ]; then
            return 1
        fi
    done
    return 0
}

# 等待进程终止，最长等待 $2 秒（默认 5 秒）
# 用法: wait_for_process_exit "process_name" [timeout_seconds]
function wait_for_process_exit() {
    local process_name="$1"
    local timeout="${2:-5}"
    local interval_ms=200
    local max_iterations=$((timeout * 1000 / interval_ms))
    local i=0

    while pgrep -x "$process_name" >/dev/null 2>&1; do
        sleep 0.2
        i=$((i + 1))
        if [ $i -ge $max_iterations ]; then
            return 1
        fi
    done
    return 0
}

# 初始化时区（在日志设置之后、用户目录初始化之前）
initialize_timezone

# ============================================================================
# 🎯 注意：所有服务均以 root 用户运行
# ============================================================================
# 不再需要 UID/GID 匹配，因为 root 用户拥有所有权限
# HOME 环境变量设置为 /home/user，缓存和配置仍存放在该目录
# ============================================================================


# ============================================================================
# 🎯 用户主目录初始化（解决挂载空目录导致的花屏和图标消失问题）
# 当宿主机目录挂载到 /home/user 时，镜像中预置的配置会被覆盖
# 此函数从骨架目录 /etc/skel-user-desktop 恢复必要的配置文件
# ============================================================================
function initialize_user_home() {
    log "Initializing user home directory..."

    local SKEL_DIR="/etc/skel-user-desktop"
    local USER_HOME="/home/user"

    # 检查骨架目录是否存在
    if [ ! -d "$SKEL_DIR" ]; then
        log_warn " Skeleton directory not found: $SKEL_DIR"
        return 1
    fi

    # 检查 /home/user 是否被外部挂载覆盖（通过检查关键目录是否存在）
    local need_restore=false

    # 检查关键目录/文件是否存在（任一不存在则需要恢复）
    # 注意：.config/xfce4 包含 XFCE Panel 配置，缺失会导致底部 Dock 栏图标消失
    if [ ! -d "$USER_HOME/Desktop" ] || \
       [ ! -f "$USER_HOME/.bashrc" ] || \
       [ ! -f "$USER_HOME/.bunfig.toml" ] || \
       [ ! -d "$USER_HOME/.claude" ] || \
       [ ! -d "$USER_HOME/.config/xfce4" ]; then
        need_restore=true
        log "Detected empty or incomplete user home directory (likely mounted)"
        echo "   Missing: $([ ! -d "$USER_HOME/Desktop" ] && echo 'Desktop ')$([ ! -f "$USER_HOME/.bashrc" ] && echo '.bashrc ')$([ ! -f "$USER_HOME/.bunfig.toml" ] && echo '.bunfig.toml ')$([ ! -d "$USER_HOME/.claude" ] && echo '.claude ')$([ ! -d "$USER_HOME/.config/xfce4" ] && echo '.config/xfce4 ')"
    fi

    if [ "$need_restore" = true ]; then
        log "Restoring user configuration from skeleton directory..."

        # 创建必要的目录结构
        mkdir -p "$USER_HOME/.config" "$USER_HOME/.local/share" "$USER_HOME/.cache"
        mkdir -p "$USER_HOME/Desktop"

        # ========== Desktop 目录 - 强制覆盖桌面图标（防止坏软链接）==========
        if [ -d "$SKEL_DIR/Desktop" ]; then
            # 先删除现有的 .desktop 文件（可能是损坏的软链接）
            rm -f "$USER_HOME/Desktop/"*.desktop 2>/dev/null || true
            # 强制复制桌面图标
            cp -a "$SKEL_DIR/Desktop/"*.desktop "$USER_HOME/Desktop/" 2>/dev/null || true
            # 设置可执行权限
            chmod +x "$USER_HOME/Desktop/"*.desktop 2>/dev/null || true
            log_success "  Desktop icons restored (forced overwrite)"
        fi

        # ========== .bashrc - 强制覆盖 ==========
        if [ -f "$SKEL_DIR/.bashrc" ]; then
            cp -a "$SKEL_DIR/.bashrc" "$USER_HOME/.bashrc"
            log_success "  .bashrc restored (forced overwrite)"
        fi

        # ========== .config 目录 - 强制覆盖（保留 Chromium 用户数据）==========
        if [ -d "$SKEL_DIR/.config" ]; then
            # 1. 备份现有的 Chromium 用户数据（书签、历史记录等）
            local chromium_backup=""
            if [ -d "$USER_HOME/.config/chromium" ]; then
                chromium_backup=$(mktemp -d)
                cp -a "$USER_HOME/.config/chromium" "$chromium_backup/" 2>/dev/null || true
                log_success "  Chromium user data backed up"
            fi

            # 2. 强制覆盖整个 .config 目录
            cp -a "$SKEL_DIR/.config/." "$USER_HOME/.config/" 2>/dev/null || true
            log_success "  .config directory restored (forced overwrite)"

            # 3. 还原 Chromium 用户数据（覆盖骨架目录的默认配置）
            if [ -n "$chromium_backup" ] && [ -d "$chromium_backup/chromium" ]; then
                cp -a "$chromium_backup/chromium/." "$USER_HOME/.config/chromium/" 2>/dev/null || true
                rm -rf "$chromium_backup"
                log_success "  Chromium user data restored"
            fi
        fi

        # ========== .local 目录 - 强制覆盖 ==========
        if [ -d "$SKEL_DIR/.local" ]; then
            cp -a "$SKEL_DIR/.local/." "$USER_HOME/.local/" 2>/dev/null || true
            log_success "  .local directory restored (forced overwrite)"
        fi

        # ========== .bunfig.toml - 强制覆盖 ==========
        if [ -f "$SKEL_DIR/.bunfig.toml" ]; then
            cp -a "$SKEL_DIR/.bunfig.toml" "$USER_HOME/.bunfig.toml"
            log_success "  .bunfig.toml restored (forced overwrite)"
        fi

        # ========== .npmrc - 强制覆盖（pnpm 配置）==========
        if [ -f "$SKEL_DIR/.npmrc" ]; then
            cp -a "$SKEL_DIR/.npmrc" "$USER_HOME/.npmrc"
            log_success "  .npmrc restored (forced overwrite)"
        fi

        # ========== .claude 目录 - 不覆盖（保留用户配置）==========
        if [ -d "$SKEL_DIR/.claude" ]; then
            mkdir -p "$USER_HOME/.claude"
            cp -an "$SKEL_DIR/.claude/." "$USER_HOME/.claude/" 2>/dev/null || true
            log_success "  .claude directory restored (preserve existing)"
        fi

        # .cache 目录 - 恢复工具缓存配置（bun, uv, pnpm）
        if [ -d "$SKEL_DIR/.cache" ]; then
            mkdir -p "$USER_HOME/.cache"
            # 只复制目录结构，不复制实际缓存内容（避免大量复制）
            for cache_subdir in bun uv pnpm; do
                if [ -d "$SKEL_DIR/.cache/$cache_subdir" ] && [ ! -d "$USER_HOME/.cache/$cache_subdir" ]; then
                    mkdir -p "$USER_HOME/.cache/$cache_subdir"
                    log_success "  .cache/$cache_subdir directory created"
                fi
            done
        fi

        log_success "User home directory initialized from skeleton"
    else
        log_success "User home directory already initialized"
    fi

    # ========== 额外保护：确保 XFCE Panel 配置始终有效 ==========
    # 即使上面的恢复逻辑没有触发，也检查 Panel 配置是否完整
    # 关键修复：XFCE 会在运行时重写 panel.xml，可能保存损坏的配置
    # 因此必须检查内容有效性，而不仅仅是文件是否存在
    local XFCE_PANEL_XML="$USER_HOME/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml"
    local XFCE_PANEL_SYSTEM="/etc/xdg/xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml"
    local panel_corrupted=false

    # 检查 panel.xml 是否存在且有效
    if [ ! -f "$XFCE_PANEL_XML" ] || [ ! -s "$XFCE_PANEL_XML" ]; then
        panel_corrupted=true
        log "XFCE Panel config missing or empty"
    elif ! grep -q 'value="launcher"' "$XFCE_PANEL_XML" 2>/dev/null; then
        # 检查 panel.xml 是否包含有效的 launcher 定义
        # 如果 plugin-17 等不是 type="string" value="launcher"，说明配置被 XFCE 重写损坏了
        panel_corrupted=true
        log "XFCE Panel config corrupted (launcher definitions missing)"
    elif ! grep -q 'xfce4-terminal-emulator.desktop' "$XFCE_PANEL_XML" 2>/dev/null; then
        # 检查是否包含 launcher items（.desktop 文件引用）
        panel_corrupted=true
        log "XFCE Panel config corrupted (launcher items empty)"
    fi

    if [ "$panel_corrupted" = true ]; then
        log "Restoring XFCE Panel config from system..."
        mkdir -p "$(dirname "$XFCE_PANEL_XML")"
        if [ -f "$XFCE_PANEL_SYSTEM" ]; then
            cp -f "$XFCE_PANEL_SYSTEM" "$XFCE_PANEL_XML"
            log_success "  xfce4-panel.xml restored from system config (forced overwrite)"
        elif [ -f "$SKEL_DIR/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml" ]; then
            cp -f "$SKEL_DIR/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml" "$XFCE_PANEL_XML"
            log_success "  xfce4-panel.xml restored from skeleton (forced overwrite)"
        fi
    else
        log_success "XFCE Panel config is valid"
    fi

    # 确保 Panel launcher 目录存在且内容完整（强制恢复）
    # 注意：每次启动都检查并恢复，防止用户删除后 XFCE 保存损坏状态
    local XFCE_PANEL_DIR="$USER_HOME/.config/xfce4/panel"
    for launcher_id in 17 18 19 20; do
        local launcher_dir="$XFCE_PANEL_DIR/launcher-$launcher_id"
        local system_launcher="/etc/xdg/xfce4/panel/launcher-$launcher_id"
        if [ -d "$system_launcher" ]; then
            # 检查 launcher 目录是否存在且包含 .desktop 文件
            if [ ! -d "$launcher_dir" ] || [ -z "$(ls -A "$launcher_dir" 2>/dev/null)" ]; then
                mkdir -p "$launcher_dir"
                cp -f "$system_launcher/"* "$launcher_dir/" 2>/dev/null || true
                log_success "  launcher-$launcher_id restored (forced)"
            fi
        fi
    done

    # ========== 确保 GTK CSS 配置存在（隐藏 Thunar root 警告） ==========
    # 同时配置 /root 和 /home/user，因为：
    # - 以 root 用户运行时，某些进程可能读取 /root/.config
    # - 设置 HOME=/home/user 后，大部分进程读取 /home/user/.config
    local GTK_CSS_CONTENT='/* Hide Thunar root warnings completely */
.thunar-window infobar.warning { min-height: 0; max-height: 0; padding: 0; margin: 0; opacity: 0; }
.thunar-window infobar.warning * { min-height: 0; max-height: 0; padding: 0; margin: 0; opacity: 0; }
.thunar-window infobar.warning button { min-height: 0; min-width: 0; max-height: 0; padding: 0; margin: 0; opacity: 0; }
infobar.warning { min-height: 0; max-height: 0; padding: 0; margin: 0; opacity: 0; }
infobar.warning * { min-height: 0; max-height: 0; padding: 0; margin: 0; opacity: 0; }
/* Hide XFCE root warning */
.root-warning { display: none !important; opacity: 0 !important; }
.root-warning * { display: none !important; opacity: 0 !important; }
'
    # 为 /root 创建配置
    mkdir -p /root/.config/gtk-3.0
    echo "$GTK_CSS_CONTENT" > /root/.config/gtk-3.0/gtk.css
    log_success "  GTK CSS created for /root"

    # 为 /home/user 创建配置
    mkdir -p "$USER_HOME/.config/gtk-3.0"
    echo "$GTK_CSS_CONTENT" > "$USER_HOME/.config/gtk-3.0/gtk.css"
    log_success "  GTK CSS created for $USER_HOME"

    # ========== 设置 Chromium 为默认浏览器（解决 xdg-open 无法打开浏览器问题）==========
    log "Configuring Chromium as default web browser..."

    # 创建用户级 mimeapps.list（强制覆盖，确保默认浏览器设置正确）
    mkdir -p "$USER_HOME/.config" "$USER_HOME/.local/share/applications"

    cat > "$USER_HOME/.config/mimeapps.list" <<'EOF'
[Default Applications]
text/html=chromium.desktop
text/xml=chromium.desktop
application/xhtml+xml=chromium.desktop
application/xml=chromium.desktop
application/rss+xml=chromium.desktop
application/rdf+xml=chromium.desktop
x-scheme-handler/http=chromium.desktop
x-scheme-handler/https=chromium.desktop
x-scheme-handler/ftp=chromium.desktop
x-scheme-handler/chrome=chromium.desktop
x-scheme-handler/about=chromium.desktop
x-scheme-handler/unknown=chromium.desktop

[Added Associations]
text/html=chromium.desktop;
x-scheme-handler/http=chromium.desktop;
x-scheme-handler/https=chromium.desktop;
EOF

    # 同时创建 ~/.local/share/applications/mimeapps.list（某些 xdg 工具读取这个位置）
    cp "$USER_HOME/.config/mimeapps.list" "$USER_HOME/.local/share/applications/mimeapps.list"

    # 使用 xdg-settings 设置默认浏览器（需要在 X11 环境启动后才能完全生效）
    # 这里先创建配置文件，xdg-settings 会在 DISPLAY 环境变量存在时使用
    export BROWSER="/usr/bin/chromium-browser-launcher"

    log_success "  Chromium set as default web browser (mimeapps.list)"
    log_success "  BROWSER env set to: $BROWSER"

    # ========== 修复挂载目录的权限（解决宿主机 UID 不匹配） ==========
    # 注意：由于 Dockerfile 中用户配置已经以 user 身份创建，
    # 这里只需要处理可能被宿主机挂载覆盖的目录
    log "Fixing permissions for mounted directories..."

    # 确保必要目录存在
    mkdir -p "$USER_HOME/.cache" /app /tmp/mesa_shader_cache "${CONTAINER_LOGS_DIR:-/app/container-logs}"

    # 修复 /home/user 目录的所有者（重要：当宿主机挂载空目录时）
    log "Fixing ownership for /home/user and mounted directories..."
    chown -R user:user "$USER_HOME" 2>/dev/null || true
    chown -R user:user /app "${CONTAINER_LOGS_DIR:-/app/container-logs}" 2>/dev/null || true
    chown -R user:user /tmp/mesa_shader_cache 2>/dev/null || true

    # 修复权限
    log "Fixing permissions..."
    chmod -R u+rwX /app "$USER_HOME/.cache" 2>/dev/null || true

    # 对于可能无法 chown 的挂载目录，尝试添加 other 权限
    chmod -R o+rX /app 2>/dev/null || true

    # 确保 bin 目录下的文件可执行
    [ -d /app/bin ] && chmod -R a+x /app/bin 2>/dev/null || true

    log_success "Permissions fixed"

    # ========== 设置渲染相关环境变量（防止花屏）==========
    # 将 Mesa 着色器缓存移到 /tmp（不受 /home/user 挂载影响）
    export MESA_SHADER_CACHE_DIR="/tmp/mesa_shader_cache"
    export MESA_GLSL_CACHE_DIR="/tmp/mesa_shader_cache"
    mkdir -p /tmp/mesa_shader_cache
    chmod 777 /tmp/mesa_shader_cache

    # 将 X 认证文件移到 /tmp
    export XAUTHORITY="/tmp/.Xauthority"

    log_success "Mesa shader cache configured: /tmp/mesa_shader_cache"
}

function start_vnc_services() {
    log "Starting VNC services (as root)..."

	# 等待X11服务完全启动
	counter=0
	while ! DISPLAY=:0 xdpyinfo >/dev/null 2>&1; do
		sleep 0.5
		let counter++
		if ((counter > 30)); then
			log "X11 not ready, skipping VNC startup"
			return 1
		fi
	done

	log "X11 is ready, starting VNC..."

	# 停止可能存在的VNC服务
	pkill x11vnc || true

	# 等待进程完全停止（智能等待，最长 3 秒）
	wait_for_process_exit "x11vnc" 3 || log_warn "x11vnc 进程终止超时"

	# 启动x11vnc服务器 (后台运行，以 root 身份)
	export DISPLAY=:0
	nohup x11vnc -bg -display :0 -forever -wait 50 -shared -rfbport 5900 -nopw 2>/tmp/x11vnc_stderr.log >/dev/null &

	# 等待 x11vnc 端口就绪（智能等待，最长 5 秒）
	if wait_for_port localhost 5900 5; then
		log_success "x11vnc port 5900 is ready"
	else
		log_warn "x11vnc port 5900 not ready within timeout"
	fi

	# 启动noVNC代理 (后台运行，以 root 身份)
	cd /opt/noVNC/utils
	nohup ./novnc_proxy --vnc localhost:5900 --listen 6080 --web /opt/noVNC > /tmp/novnc.log 2>&1 &
	cd -

	# 等待 noVNC 端口就绪（智能等待，最长 5 秒）
	if wait_for_port localhost 6080 5; then
		log_success "noVNC port 6080 is ready"
	else
		log_warn "noVNC port 6080 not ready within timeout"
	fi

	# 检查VNC服务状态
	vnc_running=false
	novnc_running=false

	# 检查x11vnc进程
	if pgrep -x x11vnc >/dev/null 2>&1; then
		vnc_running=true
		log_success "x11vnc server started on port 5900"
	else
		echo "✗ x11vnc server failed to start"
		echo "Error log:"
		cat /tmp/x11vnc_stderr.log 2>/dev/null || echo 'No error log found'
	fi

	# 检查noVNC端口
	if netstat -tuln 2>/dev/null | grep -q ":6080 "; then
		novnc_running=true
		log_success "noVNC proxy started on port 6080"
		echo "  VNC URL: http://localhost:6080/vnc.html?autoconnect=true&resize=scale"
	else
		echo "✗ noVNC proxy failed to start"
		echo "Error log:"
		cat /tmp/novnc.log 2>/dev/null || echo 'No error log found'
	fi

	if [ "$vnc_running" = true ] && [ "$novnc_running" = true ]; then
		log_success "VNC services started successfully!"
		return 0
	else
		echo "✗ VNC services failed to start properly"
		return 1
	fi
}

function start_display_and_desktop() {
    log "Starting X11 display server and XFCE4 desktop..."

	# 清理可能存在的X11锁文件和进程
	rm -f /tmp/.X0-lock /tmp/.X11-unix/X0 /tmp/.Xauthority /tmp/dbus-session-env
	pkill -f "Xvfb :0" || true
	pkill -f "xfce4-session" || true
	pkill -f "dbus-daemon" || true
	pkill -f "fcitx5" || true

    # 确保 /tmp 权限正确且 XAUTHORITY 可写
    touch /tmp/.Xauthority
    chmod 666 /tmp/.Xauthority
    touch /tmp/dbus-session-env
    chmod 666 /tmp/dbus-session-env

    # ========== 优化：尽早启动 Xvfb (后台) ==========
    # Xvfb 启动需要时间，将其提前到 Cleanup/DBus 之前，利用这段时间进行初始化
    # 色深使用 24 位，避免某些 Linux 内核上出现花屏
    log "Starting Xvfb :0 (background initialization)..."
    HOME=/home/user XAUTHORITY=/tmp/.Xauthority MESA_SHADER_CACHE_DIR=/tmp/mesa_shader_cache Xvfb :0 -ac -screen 0 1920x1080x24 -dpi 96 -nolisten tcp -nolisten unix >/dev/null 2>&1 &


	# ========== 关键修复：清理 Chromium 进程和锁文件 ==========
	log "Cleaning up stale Chromium processes and lock files..."

	# 1. 强制终止所有遗留的 Chromium 进程
	pkill -9 -f "chromium" || true
	pkill -9 -f "chrome" || true

	# 2. 设置持久化的 Chromium 数据目录路径
	# 使用用户主目录的标准配置路径（自动持久化）
	CHROMIUM_USER_DATA_DIR="${CHROMIUM_USER_DATA_DIR:-/home/user/.config/chromium}"
	log_success "使用 Chromium 数据目录: $CHROMIUM_USER_DATA_DIR (自动持久化)"

	# 3. 创建 Chromium 数据目录（如果不存在）
	mkdir -p "$CHROMIUM_USER_DATA_DIR"
	# 修复权限（已在 initialize_user_home() 中统一处理，此处确保目录存在即可）
	# 避免 chmod -R 777 造成安全风险

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

		log_success "Chromium lock files cleaned from: $CHROMIUM_USER_DATA_DIR"
	fi

	# 6. 清理 /tmp 中的 Chromium 临时文件
	rm -rf /tmp/.org.chromium.Chromium.* || true
	rm -rf /tmp/chrome_* || true

	# 7. 清理 /dev/shm 中的 Chromium 共享内存
	rm -rf /dev/shm/.org.chromium.Chromium.* || true

	log_success "Chromium cleanup completed (data dir: $CHROMIUM_USER_DATA_DIR)"

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

	# 启动 D-Bus 会话 (以 root 启动，但 HOME 设置为 /home/user)
    log "Starting D-Bus session as root (HOME=/home/user)..."
	HOME=/home/user dbus-launch --sh-syntax > /tmp/dbus-session-env

	# 等待 D-Bus 会话文件生成（智能等待，最长 3 秒）
	wait_for_file /tmp/dbus-session-env 3 || log_warn "D-Bus session file not created"

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
		log_success "D-Bus address exported to global environment"

		# ========== 关键修复：允许 root 访问 user 的 D-Bus socket ==========
		# 修改 D-Bus socket 文件权限，允许 root 用户连接（用于 MCP chromium 中文输入）
		chmod 777 /tmp/dbus-* 2>/dev/null || true
		log_success "D-Bus socket permissions updated for root access"
	fi

	# 启动 D-Bus 系统总线
    log "Starting D-Bus system bus..."
	mkdir -p /var/run/dbus
	dbus-daemon --system --fork

	# 等待 D-Bus 系统总线 socket 就绪（智能等待，最长 2 秒）
	wait_for_file /var/run/dbus/system_bus_socket 2 || log_warn "D-Bus system bus socket not ready"

	# 启动 PolicyKit 守护进程（配置为不需要认证）
    log "Starting PolicyKit daemon..."
	/usr/lib/policykit-1/polkitd --no-debug >/var/log/polkitd.log 2>&1 &

	# 等待 PolicyKit 进程启动（智能等待，最长 3 秒）
	wait_for_process "polkitd" 3 || log_warn "PolicyKit daemon not started"

    # 等待Xvfb启动（此时应该已经差不多就绪了）
    log "Waiting for X11 to be ready..."
    counter=0
    while ! DISPLAY=:0 xdpyinfo >/dev/null 2>&1; do
        sleep 0.1
        let counter++
        if ((counter > 100)); then
            echo "Failed to start Xvfb"
            return 1
        fi
    done

	# ========== 关键修复：手动启动 fcitx5，确保环境变量正确 ==========
	# 不再依赖 XFCE autostart，直接用正确的环境变量启动 (as root, HOME=/home/user)
    log "Starting fcitx5 input method (as root)..."
	env \
		HOME=/home/user \
		DISPLAY=:0 \
		DBUS_SESSION_BUS_ADDRESS="${DBUS_ADDR}" \
		LANG=C.UTF-8 \
		LC_ALL=C.UTF-8 \
		LC_CTYPE=C.UTF-8 \
		GTK_IM_MODULE=fcitx \
		QT_IM_MODULE=fcitx \
		XMODIFIERS=@im=fcitx \
		INPUT_METHOD=fcitx \
		fcitx5 -d --replace >/tmp/fcitx5-startup.log 2>&1 &

	# 等待 fcitx5 进程启动（智能等待，最长 5 秒）
	if wait_for_process "fcitx5" 5; then
		log_success "fcitx5 started successfully"
	else
		log_warn "fcitx5 failed to start, check /tmp/fcitx5-startup.log"
	fi

	# 以 root 用户启动 XFCE4 会话（但 HOME 设置为 /home/user）
	# 注意：使用 @im=fcitx 与系统 immodule 兼容
	export DISPLAY=:0
	export HOME=/home/user
	export XDG_CURRENT_DESKTOP=XFCE
	export XDG_SESSION_DESKTOP=xfce
	export XDG_RUNTIME_DIR=/run/user/0
	export GNOME_KEYRING_CONTROL=/run/user/0/keyring
	export GTK_MODULES=gnome-keyring-pkcs11
	export DBUS_SESSION_BUS_ADDRESS="${DBUS_ADDR}"
	export LANG=C.UTF-8
	export LC_ALL=C.UTF-8
	export LC_CTYPE=C.UTF-8
	export GTK_IM_MODULE=fcitx
	export QT_IM_MODULE=fcitx
	export XMODIFIERS=@im=fcitx
	export INPUT_METHOD=fcitx
	export SDL_IM_MODULE=fcitx
	export GLFW_IM_MODULE=ibus

	log "Environment variables set:"
	echo "  HOME=$HOME"
	echo "  GTK_IM_MODULE=$GTK_IM_MODULE"
	echo "  XMODIFIERS=$XMODIFIERS"
	echo "  DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS"
	echo "  LANG=$LANG"

	# 启动 gnome-keyring-daemon
	gnome-keyring-daemon --start --components=secrets,ssh,pkcs11 >/dev/null 2>&1 &

	# 启动 PolicyKit 认证代理
	/usr/lib/policykit-1-gnome/polkit-gnome-authentication-agent-1 >/var/log/polkit-agent.log 2>&1 &

	# 等待 gnome-keyring-daemon 启动（智能等待，最长 2 秒）
	wait_for_process_pattern "gnome-keyring-daemon" 2 || true

	echo 'Fcitx5 already started manually'

	# 以 root 身份启动 XFCE4，但 HOME 设置为 /home/user
	env \
		DISPLAY=:0 \
		HOME=/home/user \
		XDG_CURRENT_DESKTOP=XFCE \
		XDG_SESSION_DESKTOP=xfce \
		XDG_RUNTIME_DIR=/run/user/0 \
		DBUS_SESSION_BUS_ADDRESS="${DBUS_ADDR}" \
		LANG=C.UTF-8 \
		LC_ALL=C.UTF-8 \
		LC_CTYPE=C.UTF-8 \
		GTK_IM_MODULE=fcitx \
		QT_IM_MODULE=fcitx \
		XMODIFIERS=@im=fcitx \
		INPUT_METHOD=fcitx \
		xfce4-session &

	log "X11 display and XFCE4 desktop started successfully (as root, HOME=/home/user)"
}

# ============================================================================
# 🎯 XFCE 壁纸设置（在 XFCE 启动后动态设置）
# XFCE 会根据显示器动态生成 xfce4-desktop.xml，需要在运行时设置壁纸
# 其他配置（screensaver, power-manager, panel）已在 /etc/xdg/xfce4 系统目录中
# ============================================================================
function apply_xfce_wallpaper() {
    log "Applying XFCE wallpaper (as root)..."

    # 等待 XFCE 桌面完全启动
    local counter=0
    while ! DISPLAY=:0 HOME=/home/user xfconf-query -c xfce4-desktop -l >/dev/null 2>&1; do
        sleep 1
        let counter++
        if ((counter > 30)); then
            log_warn " Timeout waiting for XFCE desktop, skipping wallpaper"
            return 1
        fi
    done

    local WALLPAPER_PATH="/usr/share/backgrounds/xfce/wallpaper.jpeg"
    if [ ! -f "$WALLPAPER_PATH" ]; then
        log_warn " Wallpaper not found: $WALLPAPER_PATH"
        return 1
    fi

    log_success "  Setting wallpaper: $WALLPAPER_PATH"

    # 获取当前的 monitor 配置（XFCE 可能使用不同的名称）
    local monitors=$(DISPLAY=:0 HOME=/home/user xfconf-query -c xfce4-desktop -l 2>/dev/null | grep 'workspace0/last-image' | head -5)

    if [ -n "$monitors" ]; then
        # 对于每个找到的 monitor 配置设置壁纸
        echo "$monitors" | while read monitor_path; do
            DISPLAY=:0 HOME=/home/user xfconf-query -c xfce4-desktop -p "$monitor_path" -s "$WALLPAPER_PATH" 2>/dev/null || true
        done
    else
        # 直接设置常用的 monitor 路径
        DISPLAY=:0 HOME=/home/user xfconf-query -c xfce4-desktop -p /backdrop/screen0/monitorscreen/workspace0/last-image -n -t string -s "$WALLPAPER_PATH" 2>/dev/null || true
        DISPLAY=:0 HOME=/home/user xfconf-query -c xfce4-desktop -p /backdrop/screen0/monitor0/workspace0/last-image -n -t string -s "$WALLPAPER_PATH" 2>/dev/null || true
    fi

    # 设置壁纸样式（5 = 缩放）
    DISPLAY=:0 HOME=/home/user xfconf-query -c xfce4-desktop -p /backdrop/screen0/monitorscreen/workspace0/image-style -n -t int -s 5 2>/dev/null || true

    log_success "XFCE wallpaper applied successfully"
}

function check_vnc_health() {
    # 检查VNC服务健康状态 (as root)
    if [ "$VNC_AUTO_START" = "true" ]; then
        # 检查x11vnc进程
        if ! pgrep -x x11vnc >/dev/null 2>&1; then
            log_warn " x11vnc process not running, attempting restart..."
            return 1
        fi

        # 检查noVNC端口
        if ! netstat -tuln 2>/dev/null | grep -q ":6080 "; then
            log_warn " noVNC proxy not listening on port 6080, attempting restart..."
            return 1
        fi

        log_success "VNC services are healthy"
        return 0
    fi
    return 0
}

# Jupyter server function removed

# ============================================================================
# 🔊 音频流服务 (pcmflux)
# 使用 PulseAudio 虚拟声卡捕获音频，通过 WebSocket 流到浏览器
# ============================================================================
function start_audio_services() {
    log "Starting audio streaming services (pcmflux)..."

    # 1. 启动 PulseAudio 守护进程
    echo "  Starting PulseAudio daemon..."

    # 确保 PulseAudio 目录存在
    mkdir -p /home/user/.config/pulse
    mkdir -p /var/run/pulse
    chmod 777 /var/run/pulse

    # 启动 PulseAudio（非系统模式，允许运行为 root）
    HOME=/home/user pulseaudio --start --exit-idle-time=-1 --log-level=warning 2>/tmp/pulseaudio.log || true

    # 等待 PulseAudio 进程启动（智能等待，最长 3 秒）
    if wait_for_process "pulseaudio" 3; then
        log_success "  PulseAudio daemon started"
    else
        log_warn "  PulseAudio failed to start, checking log..."
        cat /tmp/pulseaudio.log 2>/dev/null || true
        # 尝试用 --system 模式启动
        pulseaudio --system --disallow-exit --disallow-module-loading=0 &
        wait_for_process "pulseaudio" 3 || log_warn "  PulseAudio system mode also failed"
    fi

    # 2. 创建虚拟声卡 (null sink) 作为音频输出目标
    echo "  Creating virtual audio sink..."
    pactl load-module module-null-sink sink_name=virtual_speaker \
          sink_properties=device.description="Virtual_Speaker" 2>/dev/null || true

    # 设置虚拟声卡为默认输出
    pactl set-default-sink virtual_speaker 2>/dev/null || true

    # 验证虚拟声卡
    if pactl list sinks short 2>/dev/null | grep -q "virtual_speaker"; then
        log_success "  Virtual speaker sink created"
    else
        log_warn "  Failed to create virtual speaker sink"
    fi

    # 3. 启动 pcmflux 音频流服务
    echo "  Starting pcmflux audio streaming service..."

    # 设置音频设备环境变量
    export AUDIO_DEVICE="virtual_speaker.monitor"
    export AUDIO_HTTP_PORT=6090
    export AUDIO_WS_PORT=6089

    # 后台启动音频服务器
    nohup python3 /usr/local/bin/audio_server.py > /tmp/audio_server.log 2>&1 &

    # 等待音频服务进程启动或端口就绪（智能等待，最长 5 秒）
    if wait_for_process_pattern "audio_server.py" 3 && wait_for_port localhost 6090 3; then
        log_success "  pcmflux audio server started"
        log_success "  Audio HTTP: http://localhost:6090"
        log_success "  Audio WebSocket: ws://localhost:6089"
    else
        log_warn "  pcmflux audio server failed to start"
        echo "  Error log:"
        cat /tmp/audio_server.log 2>/dev/null | tail -20 || true
    fi

    log_success "Audio streaming services initialized"
}

# ============================================================================
# 🔌 MCP Proxy 服务 (chrome-devtools-mcp 共享代理)
# 将 stdio 协议的 MCP 服务代理为 HTTP 服务，供多个 agent 复用
# ============================================================================
function start_mcp_proxy_services() {
    log "Starting MCP Proxy services (chrome-devtools shared)..."

    # 创建 MCP 日志目录（持久化到挂载的 /app/container-logs）
    local MCP_LOG_DIR="${CONTAINER_LOGS_DIR:-/app/container-logs}/mcp"
    mkdir -p "$MCP_LOG_DIR"
    chmod 755 "$MCP_LOG_DIR"
    log_success "  MCP log directory: $MCP_LOG_DIR"

    # MCP 配置文件路径（由 Dockerfile 复制到 /etc/mcp）
    local MCP_CONFIG_FILE="/etc/mcp/mcp-proxy-config.json"
    if [ ! -f "$MCP_CONFIG_FILE" ]; then
        log_warn "  MCP config file not found: $MCP_CONFIG_FILE"
        log_warn "  MCP Proxy services will not start"
        return 1
    fi
    log_success "  MCP config file: $MCP_CONFIG_FILE"

    # 获取 D-Bus 地址
    local DBUS_ADDR=""
    if [ -f /tmp/dbus-session-env ]; then
        source /tmp/dbus-session-env
        DBUS_ADDR="$DBUS_SESSION_BUS_ADDRESS"
    fi

    # 启动 mcp-proxy proxy 服务
    # 需要传递正确的环境变量（DISPLAY, D-Bus, 输入法等）
    echo "  Starting mcp-proxy proxy on port 18099..."
    env \
        HOME=/home/user \
        DISPLAY=:0 \
        DBUS_SESSION_BUS_ADDRESS="${DBUS_ADDR}" \
        CHROMIUM_USER_DATA_DIR=/home/user/.config/chromium \
        GTK_IM_MODULE=fcitx \
        QT_IM_MODULE=fcitx \
        XMODIFIERS=@im=fcitx \
        INPUT_METHOD=fcitx \
        LANG=C.UTF-8 \
        LC_ALL=C.UTF-8 \
        LC_CTYPE=C.UTF-8 \
        PATH="/usr/local/bin:/opt/cargo/bin:$PATH" \
        nohup mcp-proxy proxy --port 18099 --host 127.0.0.1 --config-file "$MCP_CONFIG_FILE" \
        > "$MCP_LOG_DIR/mcp-proxy.log" 2>&1 &

    local MCP_PID=$!

    # 等待 MCP Proxy 端口就绪（智能等待，最长 10 秒）
    if wait_for_port 127.0.0.1 18099 10 && kill -0 $MCP_PID 2>/dev/null; then
        log_success "  MCP Proxy started (PID: $MCP_PID)"
        log_success "  MCP Proxy URL: http://127.0.0.1:18099"
        log_success "  Agent 可使用: mcp-proxy convert http://127.0.0.1:18099"
    else
        log_warn "  MCP Proxy failed to start, check log: $MCP_LOG_DIR/mcp-proxy.log"
        cat "$MCP_LOG_DIR/mcp-proxy.log" 2>/dev/null | tail -20 || true
    fi

    log_success "MCP Proxy services initialized"
}

# ============================================================================
# ⌨️ IME 本地输入法透传服务
# 允许用户使用宿主机的输入法（如搜狗输入法）直接输入到远程桌面
# ============================================================================
function start_ime_services() {
    log "Starting IME passthrough services..."

    # 检查是否启用（可通过环境变量禁用）
    if [ "${ENABLE_IME_PASSTHROUGH:-true}" = "false" ]; then
        log_warn "  IME passthrough is disabled (ENABLE_IME_PASSTHROUGH=false)"
        return 0
    fi

    # 创建 IME 日志目录（持久化到挂载的 /app/container-logs）
    local IME_LOG_DIR="${CONTAINER_LOGS_DIR:-/app/container-logs}/ime"
    mkdir -p "$IME_LOG_DIR"
    chmod 755 "$IME_LOG_DIR"
    log_success "  IME log directory: $IME_LOG_DIR"

    # 检查 IME 服务脚本是否存在
    local IME_SCRIPT="/usr/local/bin/ime_server.py"
    if [ ! -f "$IME_SCRIPT" ]; then
        log_warn "  IME server script not found: $IME_SCRIPT"
        log_warn "  IME passthrough services will not start"
        return 1
    fi

    # 获取 D-Bus 地址
    local DBUS_ADDR=""
    if [ -f /tmp/dbus-session-env ]; then
        source /tmp/dbus-session-env
        DBUS_ADDR="$DBUS_SESSION_BUS_ADDRESS"
    fi

    # 启动 IME 服务
    echo "  Starting IME server on port 6091..."
    env \
        HOME=/home/user \
        DISPLAY=:0 \
        DBUS_SESSION_BUS_ADDRESS="${DBUS_ADDR}" \
        IME_PORT=6091 \
        IME_HOST=0.0.0.0 \
        nohup python3 "$IME_SCRIPT" \
        > "$IME_LOG_DIR/ime_server.log" 2>&1 &

    local IME_PID=$!

    # 等待 IME 服务端口就绪（智能等待，最长 5 秒）
    if wait_for_port 127.0.0.1 6091 5 && kill -0 $IME_PID 2>/dev/null; then
        log_success "  IME server started (PID: $IME_PID)"
        log_success "  IME WebSocket: ws://0.0.0.0:6091"
        log_success "  用户可使用宿主机输入法输入到远程桌面"
    else
        log_warn "  IME server failed to start, check log: $IME_LOG_DIR/ime_server.log"
        cat "$IME_LOG_DIR/ime_server.log" 2>/dev/null | tail -20 || true
    fi

    log_success "IME passthrough services initialized"
}
# 设置VNC自动启动标志
export VNC_AUTO_START=true

# ========== 关键：在启动 X11 之前初始化用户主目录 ==========
# 从骨架目录恢复配置（解决挂载空目录导致的花屏和图标消失）
initialize_user_home

# ========== MCP Proxy 服务在 X11 就绪后启动 ==========
# 注意：chrome-devtools-mcp 需要 X11 来启动 Chromium 浏览器
# 因此必须等待 Xvfb 启动后才能启动 MCP Proxy
# MCP Proxy 的启动已移动到下方的 VNC 后台任务中（X11 就绪后）

# 首先启动显示服务和桌面环境
start_display_and_desktop &

# 设置全局DISPLAY环境变量
export DISPLAY=:0
echo "DISPLAY=:0" >> /etc/environment

# envd 服务已删除 - 不再启动环境守护进程

# Jupyter services removed

# 启动 VNC 服务（在后台运行，等待X11就绪）
log "Starting VNC services in background (as root)..."
log "VNC will be available at: http://localhost:6080/vnc.html?autoconnect=true&resize=scale"
(
    # 等待X11服务就绪
    counter=0
    while ! DISPLAY=:0 xdpyinfo >/dev/null 2>&1; do
        sleep 1
        let counter++
        if ((counter > 60)); then
            echo "Timeout waiting for X11, VNC services will not start"
            exit 1
        fi
    done

    log "X11 is ready, starting services in parallel..."

    # ========== 并行启动所有依赖 X11 的服务 ==========
    # 这些服务互不依赖，可以同时启动以缩短整体启动时间

    # 1. VNC 服务（后台）
    (
        start_vnc_services
        log_success "VNC services started successfully!"
        log_success "VNC URL: http://localhost:6080/vnc.html?autoconnect=true&resize=scale"
        log_success "Direct VNC port: 5900"
    ) &
    vnc_pid=$!

    # 2. MCP Proxy 服务（后台）- 需要 X11 来启动 Chromium
    (
        log "Starting MCP Proxy services..."
        start_mcp_proxy_services
    ) &
    mcp_pid=$!

    # 3. 音频流服务 (pcmflux)（后台）
    (
        start_audio_services
    ) &
    audio_pid=$!

    # 4. IME 本地输入法透传服务（后台）
    (
        start_ime_services
    ) &
    ime_pid=$!

    # 5. 应用 XFCE 壁纸（后台）
    (
        apply_xfce_wallpaper
    ) &
    wallpaper_pid=$!

    # 等待所有并行服务启动完成
    log "Waiting for all services to start..."
    wait $vnc_pid $mcp_pid $audio_pid $ime_pid $wallpaper_pid 2>/dev/null || true
    log_success "All X11-dependent services started!"

    # VNC服务监控循环 (as root)
    while true; do
        sleep 30
        if ! check_vnc_health; then
            echo "VNC服务异常，正在重启..."
            # 停止现有服务
            pkill x11vnc || true
            pkill -f novnc_proxy || true
            # 等待进程终止（智能等待，最长 3 秒）
            wait_for_process_exit "x11vnc" 3 || true
            # 重新启动VNC服务
            start_vnc_services
        fi
    done
) &

# 启动 agent_runner 服务，支持从环境变量读取端口
log "Starting agent_runner service on port ${PORT:-8086}..."

# ========== 关键修复：确保 agent_runner 及其子进程继承输入法环境 ==========
# 从 /tmp/dbus-session-env 加载 D-Bus 地址
# ========== 创建全局输入法环境配置文件 ==========
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

# 加载 D-Bus 会话地址并追加到环境配置文件
if [ -f /tmp/dbus-session-env ]; then
    source /tmp/dbus-session-env
    export DBUS_SESSION_BUS_ADDRESS
    log_success "agent_runner will use D-Bus: $DBUS_SESSION_BUS_ADDRESS"

    # 将 D-Bus 地址追加到全局环境配置（注意：用 >> 追加，不是覆盖）
    echo "export DBUS_SESSION_BUS_ADDRESS='${DBUS_SESSION_BUS_ADDRESS}'" >> /etc/profile.d/ime-env.sh
fi

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

log_success "Input method environment variables exported globally (/etc/profile.d/ime-env.sh)"

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

log_success "Input method environment variables written to /etc/environment"

# ============================================================================
# 🎯 启动 agent_runner
# ============================================================================

# 切换到用户主目录
cd /home/user

# 关键：显式设置 HOME 环境变量为 /home/user
# 虽然以 root 运行，但缓存和配置文件仍使用 /home/user 目录（挂载目录）
export HOME=/home/user

# 确保所有输入法环境变量已导出
source /etc/profile.d/ime-env.sh 2>/dev/null || true

# 等待 D-Bus 会话文件创建（智能等待，最长 5 秒）
wait_for_file /tmp/dbus-session-env 5 || log_warn "D-Bus session file not ready"


# ========== 等待 MCP Proxy 服务就绪 ==========
# MCP Proxy 已在后台并行启动，这里只需等待端口就绪
# 由于是并行启动，通常很快就会就绪
log "Waiting for MCP Proxy service to be ready..."
MCP_PROXY_PORT=18099
MCP_PROXY_TIMEOUT=30  # 并行启动后，超时时间从 60s 降至 30s

# 使用 wait_for_port 智能等待端口就绪
if wait_for_port 127.0.0.1 $MCP_PROXY_PORT $MCP_PROXY_TIMEOUT; then
    # 端口就绪后，进一步验证 MCP 服务是否真正可用（可选）
    MCP_TEST_RESULT=$(echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"healthcheck","version":"1.0"}}}' | \
        timeout 5 mcp-proxy convert http://127.0.0.1:$MCP_PROXY_PORT --quiet 2>/dev/null | head -1)

    if echo "$MCP_TEST_RESULT" | grep -q '"result"'; then
        log_success "MCP Proxy is fully ready on port $MCP_PROXY_PORT"
    else
        log_warn "MCP Proxy port is open but service not fully initialized, continuing anyway"
    fi
else
    log_warn "MCP Proxy not ready after ${MCP_PROXY_TIMEOUT}s, starting agent_runner anyway"
    log_warn "Agent may need to retry MCP connections on first use"
fi

# 加载 D-Bus 会话环境
if [ -f /tmp/dbus-session-env ]; then
    source /tmp/dbus-session-env
    log_success "Loaded D-Bus session: $DBUS_SESSION_BUS_ADDRESS"
fi

# 构建环境变量导出命令
ENV_EXPORTS="export HOME=/home/user; \
export DISPLAY=:0; \
export DBUS_SESSION_BUS_ADDRESS='${DBUS_SESSION_BUS_ADDRESS:-}'; \
export GTK_IM_MODULE=fcitx; \
export QT_IM_MODULE=fcitx; \
export XMODIFIERS=@im=fcitx; \
export INPUT_METHOD=fcitx; \
export LANG=C.UTF-8; \
export LC_ALL=C.UTF-8; \
export BROWSER=/usr/bin/chromium-browser-launcher; \
export PATH=/usr/local/bin:/opt/cargo/bin:\$PATH"

# 如果命令行传递了参数，则执行该参数（以 root 身份，但 HOME=/home/user）
# 否则执行默认的 agent_runner
if [ $# -gt 0 ]; then
    log "Running custom command as root (HOME=/home/user): $*"
    exec /bin/bash -c "$ENV_EXPORTS; exec $*"
else
    # 默认启动 agent_runner (以 root 身份，但 HOME=/home/user)
    log "Launching agent_runner as root (HOME=/home/user) on port ${PORT:-8086}..."
    exec /bin/bash -c "$ENV_EXPORTS; exec /usr/local/bin/agent_runner -p ${PORT:-8086}"
fi

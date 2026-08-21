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
# 🎯 加载部署模式 extra (Docker Compose / K8s 差异函数, 如 fix_mount_permissions)
# 提供 fix_mount_permissions: Docker 修权限 / K8s no-op (PVC 由 fsGroup 管理)
# DEPLOY_MODE 由 rcoder 创建容器时注入 (kubernetes_runtime.rs / agent_container_starter.rs)
# ============================================================================
source /usr/local/bin/start-up-common.sh


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

# 等待 noVNC WebSocket 服务真正就绪
# 通过 curl 发送 WebSocket 升级请求，验证 /websockify 路径可用
# 比 HTTP 检查更可靠（HTTP 200 ≠ WebSocket 代理可用）
# 比 Python websockets 更轻量（无需启动 Python 解释器）
# 用法: wait_for_novnc_ready [timeout_seconds]
function wait_for_novnc_ready() {
    local timeout="${1:-10}"
    local i=0

    log "Waiting for noVNC WebSocket service to be fully ready..."

    while true; do
        # 发送 WebSocket 升级请求到 /websockify 路径
        # 返回 101 Switching Protocols 说明 WebSocket 代理真正可用
        #
        # 注意：不要加 || echo "000"！
        # curl 收到 101 后连接升级为 WebSocket，--max-time 超时导致退出码非零
        # 如果加了 ||，echo 的输出会拼在 curl -w 输出后面，变成 "101000" 而非 "101"
        # curl 本身在连接失败时 -w "%{http_code}" 已经会输出 "000"，不需要 fallback
        local http_code
        http_code=$(curl -s -o /dev/null -w "%{http_code}" \
            --max-time 2 \
            -H "Upgrade: websocket" \
            -H "Connection: Upgrade" \
            -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" \
            -H "Sec-WebSocket-Version: 13" \
            "http://localhost:6080/websockify" 2>/dev/null)

        if [ "$http_code" = "101" ]; then
            log_success "noVNC WebSocket service is ready (WebSocket upgrade 101)"
            return 0
        fi

        sleep 0.5
        i=$((i + 1))
        if [ $i -ge $((timeout * 2)) ]; then
            log_warn "noVNC WebSocket service not ready within ${timeout}s timeout (last HTTP code: $http_code)"
            return 1
        fi
    done
}

# 注：原 check_xvnc_rfb_ready（nc 连 5900 读 RFB 版本串后断开）已移除——
# 这种 RFB 半握手会被 TigerVNC 计为"安全失败"，累积触发 "Too many security failures"
# 锁定，拒绝前端正常连接（websockify 报 Target closed）。
# Xvnc 卡死/僵尸改由 check_vnc_health 的 xdpyinfo（X11 层，走 X 协议不碰 RFB）检测。

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
# 🧹 清理旧的 VNC 就绪标记文件（容器重启时可能残留）
# 确保 VNC 状态查询 API 返回准确的状态
# ============================================================================
rm -f /tmp/vnc_ready /tmp/novnc_port_ready /tmp/wallpaper_ready
log "Cleaned up stale VNC ready markers (if any)"

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
            # 强制复制桌面图标 (-L 复制实体, 规避 rsync munge 死链 + xfdesktop untrusted)
            cp -aL "$SKEL_DIR/Desktop/"*.desktop "$USER_HOME/Desktop/" 2>/dev/null || true
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

        # ========== 清除残留的 sudo-supervisord autostart ==========
        # 老镜像 Dockerfile.old 曾写入 $USER_HOME/.config/autostart/supervisord.desktop
        # (Exec=sudo supervisord); 当前镜像不再生成, 但 /home/user 是持久化 PVC, 旧文件残留。
        # XFCE 启动会 autostart 它 → sudo 净化环境 → 那把 supervisord 解析 pgweb.conf
        # %(ENV_PGWEB_PORT)s 失败(error.log 噪音) + 第二把 supervisord 隐患(socket 冲突)。
        # 必须在 start_display_and_desktop(XFCE 起来)前删掉。定向清理, 不动其它 autostart。
        rm -f "$USER_HOME/.config/autostart/supervisord.desktop" /usr/local/bin/start-supervisord.sh 2>/dev/null || true

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

    # ========== 无条件确保 .npmrc 存在（国内镜像源）==========
    # /home/user 被宿主机挂载时 .npmrc 会丢失，且上方 need_restore 检测条件不含 .npmrc
    # （只要 Desktop/.bashrc/.bunfig.toml/.claude/.config 存在就不会触发恢复）
    # 因此每次启动都强制恢复，确保 user 用户使用国内 npm 源
    # 注意：即使此文件丢失，/usr/etc/npmrc 系统级配置仍可兜底（见 Dockerfile.base）
    if [ -f "$SKEL_DIR/.npmrc" ]; then
        cp -a "$SKEL_DIR/.npmrc" "$USER_HOME/.npmrc"
        chown user:user "$USER_HOME/.npmrc" 2>/dev/null || true
        log_success "  .npmrc ensured (国内镜像源 registry=https://registry.npmmirror.com)"
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

    # ========== 🔥 关键修复：先替换壁纸文件，再加载配置（解决壁纸竞态条件） ==========
    # 必须在 xfce4-desktop.xml 加载之前完成壁纸文件替换！
    # 否则 xfdesktop 启动时会读取到默认壁纸并缓存，后续设置不生效
    local CUSTOM_WALLPAPER="/app/assets/wallpaper.jpeg"
    local XFCE_WALLPAPER="/usr/share/backgrounds/xfce/wallpaper.jpeg"
    local NOVNC_BG="/opt/noVNC/app/images/bg.jpg"

    if [ -f "$CUSTOM_WALLPAPER" ]; then
        log "Found custom wallpaper at $CUSTOM_WALLPAPER, applying BEFORE xfdesktop starts..."

        # 1. 应用到 XFCE 桌面（备份原始文件）
        if [ ! -f "${XFCE_WALLPAPER}.original" ]; then
            cp "$XFCE_WALLPAPER" "${XFCE_WALLPAPER}.original" 2>/dev/null || true
        fi
        cp "$CUSTOM_WALLPAPER" "$XFCE_WALLPAPER"
        log_success "  XFCE wallpaper applied EARLY: $CUSTOM_WALLPAPER -> $XFCE_WALLPAPER"

        # 2. 应用到 noVNC 网页背景（同一图片，不同文件名）
        if [ ! -f "${NOVNC_BG}.original" ]; then
            cp "$NOVNC_BG" "${NOVNC_BG}.original" 2>/dev/null || true
        fi
        cp "$CUSTOM_WALLPAPER" "$NOVNC_BG"
        log_success "  noVNC background applied EARLY: $CUSTOM_WALLPAPER -> $NOVNC_BG"
    else
        log "No custom wallpaper found at $CUSTOM_WALLPAPER (using defaults)"
    fi

    # ========== 修复：预加载 XFCE 桌面壁纸配置（防止启动黑屏和缩放问题） ==========
    # 复制系统配置到用户目录，包含所有 monitor 路径和缩放设置 (image-style=5)
    local XFCE_DESKTOP_XML="$USER_HOME/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-desktop.xml"
    local XFCE_DESKTOP_SYSTEM="/etc/xdg/xfce4/xfconf/xfce-perchannel-xml/xfce4-desktop.xml"

    if [ ! -f "$XFCE_DESKTOP_XML" ] && [ -f "$XFCE_DESKTOP_SYSTEM" ]; then
        mkdir -p "$(dirname "$XFCE_DESKTOP_XML")"
        cp -f "$XFCE_DESKTOP_SYSTEM" "$XFCE_DESKTOP_XML"
        log_success "  xfce4-desktop.xml pre-configured from system (fixes wallpaper scaling)"
    fi

    # ========== 无条件补齐桌面系统图标 (每次启动确保预设图标存在) ==========
    # 与下方 launcher 补齐同理, 属于"系统预设, 每次启动确保存在"。
    # 语义: 只补缺失的, 不删除用户自定义图标, 不覆盖用户改过的同名图标:
    #   - 用户删了图标 (链接不存在)              → 补
    #   - 链接损坏 (目标无效, -e 跟踪失败)        → 补
    #   - 用户保留或改过 (存在且有效)             → 不动
    # 背景: /home/user 是持久化 PVC (rcoder-computer-workspace/{user_id}),
    #       用户删图标会被 PVC 保留; 而上方 need_restore 只在"目录不存在"时触发,
    #       删图标后 Desktop 目录还在 → 不触发 → 桌面图标必须靠这里无条件补齐才能恢复。
    if [ -d "$SKEL_DIR/Desktop" ]; then
        for skel_icon in "$SKEL_DIR/Desktop/"*.desktop; do
            [ -e "$skel_icon" ] || continue            # glob 无匹配时跳过
            local icon_name="$(basename "$skel_icon")"
            local user_icon="$USER_HOME/Desktop/$icon_name"
            if [ ! -e "$user_icon" ]; then              # 不存在或链接损坏 → 补
                rm -f "$user_icon" 2>/dev/null || true      # 清 /rsyncd-munged/ 等历史断链 (cp -a 跟随断链 dst 会失败)
                mkdir -p "$USER_HOME/Desktop"
                cp -aL "$skel_icon" "$user_icon"           # -L 复制实体 (符号链接会被 munge 死链 + xfdesktop untrusted)
                chmod +x "$user_icon" 2>/dev/null || true
                chown user:user "$user_icon" 2>/dev/null || true
                log_success "  Desktop icon ensured (was missing): $icon_name"
            fi
        done
    fi

    # 无条件确保桌面图标可执行 + 属主 (X 启动前; 补齐段只补缺失/断链,
    # rsync 同步带过来的旧图标 (644) 会跳过补齐 → xfdesktop 读到 644 弹 mark executable。
    # 这里无条件 chmod 兜底, 保证 xfdesktop 启动时桌面图标一定 755)
    if [ -d "$USER_HOME/Desktop" ]; then
        chmod 755 "$USER_HOME/Desktop/"*.desktop 2>/dev/null || true
        chown user:user "$USER_HOME/Desktop/"*.desktop 2>/dev/null || true
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
    # 注意：只使用 GTK 3.0 支持的 CSS 属性，避免解析警告
    local GTK_CSS_CONTENT='/* Hide Thunar root warnings - GTK 3.0 compatible */
.thunar-window infobar.warning {
    min-height: 0;
    padding: 0;
    margin: 0;
    opacity: 0;
}
.thunar-window infobar.warning * {
    min-height: 0;
    padding: 0;
    margin: 0;
    opacity: 0;
}
.thunar-window infobar.warning button {
    min-height: 0;
    min-width: 0;
    padding: 0;
    margin: 0;
    opacity: 0;
}
infobar.warning {
    min-height: 0;
    padding: 0;
    margin: 0;
    opacity: 0;
}
infobar.warning * {
    min-height: 0;
    padding: 0;
    margin: 0;
    opacity: 0;
}
/* Hide XFCE root warning - using opacity instead of display */
.root-warning {
    opacity: 0 !important;
    min-height: 0;
}
.root-warning * {
    opacity: 0 !important;
    min-height: 0;
}
'
    # 为 /root 创建配置
    mkdir -p /root/.config/gtk-3.0
    echo "$GTK_CSS_CONTENT" > /root/.config/gtk-3.0/gtk.css
    log_success "  GTK CSS created for /root"

    # 为 /home/user 创建配置
    mkdir -p "$USER_HOME/.config/gtk-3.0"
    echo "$GTK_CSS_CONTENT" > "$USER_HOME/.config/gtk-3.0/gtk.css"
    log_success "  GTK CSS created for $USER_HOME"

    # ========== 抑制 gnome-keyring 模块加载告警 ==========
    # 容器中未安装 gnome-keyring，配置 GTK 不尝试加载该模块
    # 创建 GTK 模块配置，禁用 gnome-keyring-pkcs11
    mkdir -p /root/.config/gtk-3.0
    cat > /root/.config/gtk-3.0/settings.ini <<'EOF'
[Settings]
gtk-modules=
EOF

    mkdir -p "$USER_HOME/.config/gtk-3.0"
    cat > "$USER_HOME/.config/gtk-3.0/settings.ini" <<'EOF'
[Settings]
gtk-modules=
EOF
    log_success "  GTK module config created (gnome-keyring disabled)"

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

    # 确保必要目录存在 (两类部署通用)
    mkdir -p "$USER_HOME/.cache" /app /tmp/mesa_shader_cache "${CONTAINER_LOGS_DIR:-/app/container-logs}"

    # 修复挂载目录权限: 由 start-up-common.sh 按 DEPLOY_MODE 提供的 fix_mount_permissions 执行
    #   - Docker Compose (bind mount): chown/chmod 修被宿主机改坏的 owner
    #   - K8s (PVC subPath): no-op, 权限由 fsGroup 管理
    fix_mount_permissions "$USER_HOME"

    # ========== 保护敏感目录（如果存在）==========
    # 确保 SSH 私钥等敏感文件权限严格
    if [ -d "$USER_HOME/.ssh" ]; then
        chmod 700 "$USER_HOME/.ssh" 2>/dev/null || true
        find "$USER_HOME/.ssh" -type f -exec chmod 600 {} \; 2>/dev/null || true
        log_success "  .ssh directory protected (700/600)"
    fi

    # ========== /app 目录权限（已在 Dockerfile 中设置，无需 chown） ==========
    # 只确保 bin 目录可执行（使用 find 避免 glob 展开问题）
    if [ -d /app/bin ]; then
        find /app/bin -type f -exec chmod a+x {} + 2>/dev/null || true
        log_success "  /app/bin executables set"
    fi

    # ========== Mesa 着色器缓存（使用 755 权限，符合最小权限原则） ==========
    chmod 755 /tmp/mesa_shader_cache 2>/dev/null || true

    log_success "Permissions fixed (optimized - no recursive chown on large directories)"

    # ========== 设置渲染相关环境变量（防止花屏）==========
    # 将 Mesa 着色器缓存移到 /tmp（不受 /home/user 挂载影响）
    export MESA_SHADER_CACHE_DIR="/tmp/mesa_shader_cache"
    export MESA_GLSL_CACHE_DIR="/tmp/mesa_shader_cache"
    # 注意：目录创建和权限设置已在上面的权限修复部分完成 (Line 473, 516)

    # 将 X 认证文件移到 /tmp
    export XAUTHORITY="/tmp/.Xauthority"

    log_success "Mesa shader cache configured: /tmp/mesa_shader_cache"

    # ========== 🖼️ 自定义壁纸替换已提前执行 ==========
    # 壁纸替换逻辑已移到 xfce4-desktop.xml 加载之前执行（见本函数开头）
    # 这样可以确保 xfdesktop 启动时就能读取到正确的壁纸文件
}

function start_vnc_services() {
    log "Starting VNC services (as root)..."

	# 注意：调用方（主启动子 shell）已通过 xdpyinfo 确认 X11 就绪
	# 此处不再重复等待 X11，直接检查 Xvnc 端口

	# 1. 等待 Xvnc 端口 5900 就绪（Xvnc 在 start_display_and_desktop 中启动）
	#    如果 5900 不可用，noVNC 代理必然连接失败，直接 fail fast
	if wait_for_port localhost 5900 5; then
		log_success "Xvnc port 5900 is ready"
	else
		log_error "Xvnc port 5900 not ready within timeout, cannot start noVNC"
		return 1
	fi

	# 2. 启动 noVNC 代理（使用绝对路径，避免 cd 污染日志输出）
	if ! pgrep -f "novnc_proxy" >/dev/null 2>&1; then
		nohup /opt/noVNC/utils/novnc_proxy \
			--vnc localhost:5900 \
			--listen 6080 \
			--web /opt/noVNC \
			--heartbeat 30 \
			> /tmp/novnc.log 2>&1 &
	fi

	# 3. 等待 noVNC 端口就绪（最长 20 秒）
	if ! wait_for_port localhost 6080 20; then
		log_error "noVNC port 6080 not ready within timeout"
		log_error "noVNC error log:"
		tail -20 /tmp/novnc.log 2>/dev/null || echo 'No error log found'
		return 1
	fi
	log_success "noVNC port 6080 is listening"

	# 4. 验证 WebSocket 代理真正可用（最长 10 秒）
	#    这是写入 novnc_port_ready 标记的硬性前置条件
	#    如果 WebSocket 升级 (101) 不通过，说明 /websockify 路径不可用
	#    此时绝不能写入标记，否则前端会认为 novnc_ready=true 但实际连不上
	if ! wait_for_novnc_ready 10; then
		log_error "noVNC WebSocket service not ready, will NOT write ready marker"
		log_error "noVNC error log:"
		tail -20 /tmp/novnc.log 2>/dev/null || echo 'No error log found'
		return 1
	fi
	log_success "noVNC WebSocket service is fully ready"

	# 5. 所有检查通过，写入 noVNC 端口就绪标记
	# 此标记仅在以下条件全部满足时写入：
	#   - Xvnc 端口 5900 就绪 (VNC 服务器可用)
	#   - noVNC 端口 6080 就绪 (HTTP 服务可用)
	#   - WebSocket /websockify 升级返回 101 (浏览器可以真正连接)
	# 最终的 /tmp/vnc_ready 由 wait_and_write_vnc_ready_marker() 在壁纸+桌面也就绪后写入
	log_success "Xvnc server is running on port 5900"
	log_success "noVNC proxy started on port 6080"
	echo "  VNC URL: http://localhost:6080/vnc.html?autoconnect=true&resize=scale"
	echo "$(date +%s)" > /tmp/novnc_port_ready
	log_success "noVNC port ready marker written to /tmp/novnc_port_ready"
	return 0
}

# ========== 桌面图标信任化 ==========
# xfdesktop 4.18 对桌面 launcher 的信任只看文件属性: 实体文件 + 属主 user + 可执行 755。
# 不要写 gio metadata (metadata::trusted / xfce-exe-checksum) —— 实测反而有害:
#   xfce-exe-checksum 校验时我们只能写 sha256(整个文件), 而 xfdesktop 期望 sha256(Exec 行),
#   两者不等 → xfdesktop 判定 "launcher 被篡改" → 双击概率性弹 untrusted (还叠加写 metadata 与
#   xfdesktop 读取的时序竞态)。清空 metadata、只靠文件属性后, 确定性不弹。
# 故这里只保证文件属性 (骨架恢复 cp -aL 已给实体, 本函数兜底 chown/chmod), 不碰 gio、不重启 xfdesktop。
function trust_desktop_icons() {
    local f
    # 1) 实体 + 属主 user(uid 1000) + 可执行 755  (XFCE 4.18 五件套 条件 1+2)
    for f in /home/user/Desktop/*.desktop; do
        [ -f "$f" ] || continue
        chown user:user "$f" 2>/dev/null || true
        chmod 755 "$f" 2>/dev/null || true
    done

    # 2) gio set metadata::xfce-exe-checksum + metadata::trusted  (五件套 条件 4+5)
    #    xfdesktop 4.18 + GLib 2.74 桌面 launcher 信任要 5 条件全满足 (ef0bcdc 实测 chromium 五件套齐即不弹):
    #      uid(user) + 755 + DBUS_SESSION + xfce-exe-checksum + trusted。
    #    7a39a79 误判"只靠文件属性"删了 gio → 仅老图标靠 PVC 残留旧 metadata 不弹, 新图标/新 PVC 必弹。此处恢复。
    #    gio set 要 D-Bus session 连上 gvfsd-metadata; 启动早期没就绪会静默失败 → 先探测 gvfsd (最长 30s) 再批量写。
    [ -f /tmp/dbus-session-env ] && . /tmp/dbus-session-env
    export HOME=/home/user
    local _first _gvfs_ready=0 i
    _first=$(ls /home/user/Desktop/*.desktop 2>/dev/null | head -1)
    if [ -n "$_first" ] && [ -n "$DBUS_SESSION_BUS_ADDRESS" ]; then
        for i in $(seq 1 60); do
            gio set "$_first" metadata::trusted true 2>/dev/null && { _gvfs_ready=1; break; }
            sleep 0.5
        done
    fi
    if [ "$_gvfs_ready" = "1" ]; then
        for f in /home/user/Desktop/*.desktop; do
            [ -f "$f" ] || continue
            local cksum=$(sha256sum "$f" 2>/dev/null | cut -d' ' -f1)
            [ -n "$cksum" ] && gio set "$f" metadata::xfce-exe-checksum "$cksum" 2>/dev/null || true
            gio set "$f" metadata::trusted true 2>/dev/null || true
        done
        log_success "Desktop icons trusted (XFCE 4.18 五件套: uid+755+DBUS+checksum+trusted)"
        # 3) 刷新 xfdesktop 缓存: gio set 写在 xfdesktop 启动之后, 它已缓存旧信任状态;
        #    用 session env 重启 xfdesktop 让它重读 metadata (aaf10f4 的 refresh, 7a39a79 误删)。
        if pgrep -x xfdesktop >/dev/null 2>&1; then
            pkill -x xfdesktop 2>/dev/null || true
            sleep 1
            (env DISPLAY=:0 HOME=/home/user XDG_CURRENT_DESKTOP=XFCE \
                 DBUS_SESSION_BUS_ADDRESS="$DBUS_SESSION_BUS_ADDRESS" \
                 nohup xfdesktop >/tmp/xfdesktop-trust.log 2>&1 &)
            log "xfdesktop restarted to refresh desktop icon trust"
        fi
    else
        log_warn "gvfs metadata 后端未就绪, gio set 跳过 (图标可能双击弹 untrusted; metadata 持久化在 PVC, 下次启动恢复)"
    fi

    # 4) 兜底: 禁 Thunar 启动确认 (Thunar 文件管理器内双击 .desktop 时生效)
    if command -v xfconf-query >/dev/null 2>&1; then
        HOME=/home/user xfconf-query -c thunar -p /misc-executable-launch-confirm -s false 2>/dev/null || true
    fi
}

function start_display_and_desktop() {
    log "Starting X11 display server and XFCE4 desktop..."

	# 清理可能存在的X11锁文件和进程
	rm -f /tmp/.X0-lock /tmp/.X11-unix/X0 /tmp/.Xauthority /tmp/dbus-session-env
	pkill -f "Xvnc :0" || true
	pkill -f "xfce4-session" || true
	pkill -f "dbus-daemon" || true
	pkill -f "fcitx5" || true

    # 确保 /tmp 权限正确且 XAUTHORITY 可写
    touch /tmp/.Xauthority
    chmod 666 /tmp/.Xauthority
    touch /tmp/dbus-session-env
    chmod 666 /tmp/dbus-session-env

    # ========== 优化：尽早启动 Xvnc (后台) ==========
    # Xvnc 是 TigerVNC 内置的 X server + VNC 服务器
    # 使用 Xvnc 替代 Xvfb + x11vnc 的组合，简化架构
    # 色深使用 24 位，避免某些 Linux 内核上出现花屏
    # FrameRate 30: 限制每秒最大帧数 (默认60)，降低到30可减少约50%带宽，日常使用无明显差异
    # 注意: CompressLevel/QualityLevel 是 VNC 客户端参数，不是 Xvnc 服务端参数
    #       真正的压缩配置在 noVNC 客户端侧 (rfb.js 的 compressionLevel/qualityLevel)
    log "Starting Xvnc :0 (background initialization)..."
    HOME=/home/user XAUTHORITY=/tmp/.Xauthority MESA_SHADER_CACHE_DIR=/tmp/mesa_shader_cache Xvnc :0 -geometry 1400x1050 -depth 24 -SecurityTypes None -ac -rfbport 5900 -FrameRate 20 -AlwaysShared >/tmp/xvnc.log 2>&1 &


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

	# 4.1 agent-browser 默认配置来自 Dockerfile ENV
	log_success "agent-browser configured for shared profile: $AGENT_BROWSER_PROFILE"

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

	# ========== 优化：D-Bus system bus + PolicyKit 后台启动 ==========
	# 这两个服务不是 VNC/XFCE 的前置依赖，串行等待会浪费 ~10 秒
	# 改为后台启动，不阻塞 Xvnc → fcitx5 → xfdesktop 的关键路径
	(
		log "Starting D-Bus system bus..."
		mkdir -p /var/run/dbus
		dbus-daemon --system --fork

		if wait_for_file /var/run/dbus/system_bus_socket 5; then
			log_success "D-Bus system bus socket ready"
		else
			log_warn "D-Bus system bus socket not ready"
		fi

		log "Starting PolicyKit daemon..."
		/usr/lib/policykit-1/polkitd --no-debug >/var/log/polkitd.log 2>&1 &

		if wait_for_process "polkitd" 5; then
			log_success "PolicyKit daemon started"
		else
			log_warn "PolicyKit daemon not started"
		fi
	) &

    # 等待Xvnc启动（此时应该已经差不多就绪了）
    log "Waiting for X11 to be ready..."
    counter=0
    while ! DISPLAY=:0 xdpyinfo >/dev/null 2>&1; do
        sleep 0.1
        let counter++
        if ((counter > 100)); then
            echo "Failed to start Xvnc"
            return 1
        fi
    done

    # ========== 优化：设置初始背景色（避免纯黑死屏） ==========
    # 在壁纸加载前，将 X root 窗口设置为深灰色，提供视觉反馈
    DISPLAY=:0 xsetroot -solid "#1e1e1e" 2>/dev/null || true
    log_success "X root window color set to dark grey"

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

	# ========== 优化：预启动 xfdesktop（壁纸渲染进程）==========
	# xfdesktop 负责渲染桌面壁纸，默认在 xfce4-session 串行启动序列的后面
	# 手动预启动可以让壁纸渲染提前开始，减少 VNC 打开时的黑屏时间
	log "Pre-starting xfdesktop for faster wallpaper rendering..."
	env \
		DISPLAY=:0 \
		HOME=/home/user \
		XDG_CURRENT_DESKTOP=XFCE \
		DBUS_SESSION_BUS_ADDRESS="${DBUS_ADDR}" \
		xfdesktop &

	# 智能等待 xfdesktop 进程启动（最长 5 秒，每 200ms 检测一次）
	if wait_for_process "xfdesktop" 5; then
		log_success "xfdesktop pre-started successfully"
	else
		log_warn "xfdesktop pre-start not detected, xfce4-session will start it"
	fi

	# 以 root 身份启动 XFCE4 会话（xfce4-session 会检测到 xfdesktop 已在运行，不会重复启动）
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

    # ========== 1. 等待 xfdesktop 进程启动 ==========
    # xfdesktop 已在 start_display_and_desktop() 中预启动
    # 这里只需要短暂等待确认它已运行（最多 30 秒，兼容慢速云服务器）
    log "Waiting for xfdesktop process..."
    if wait_for_process "xfdesktop" 30; then
        log_success "xfdesktop process is running"
    else
        log_warn "xfdesktop not detected after 30s, continuing anyway (xfce4-session may start it)"
    fi

    # ========== 2. 等待 xfconf-query 可用 ==========
    local counter=0
    while ! DISPLAY=:0 HOME=/home/user xfconf-query -c xfce4-desktop -l >/dev/null 2>&1; do
        sleep 0.3
        ((counter++))
        if ((counter > 30)); then
            log_warn " Timeout waiting for XFCE desktop xfconf, skipping wallpaper"
            return 1
        fi
    done

    # 壁纸路径：支持通过环境变量 CUSTOM_WALLPAPER_PATH 自定义
    # 如果自定义壁纸不存在，使用容器内的默认壁纸
    local CUSTOM_WALLPAPER="${CUSTOM_WALLPAPER_PATH:-}"
    if [ -n "$CUSTOM_WALLPAPER" ] && [ -f "$CUSTOM_WALLPAPER" ]; then
        local WALLPAPER_PATH="$CUSTOM_WALLPAPER"
        log "Using custom wallpaper: $WALLPAPER_PATH"
    else
        local WALLPAPER_PATH="/usr/share/backgrounds/xfce/wallpaper.jpeg"
        if [ -n "$CUSTOM_WALLPAPER" ]; then
            log_warn " Custom wallpaper not found: $CUSTOM_WALLPAPER, using default"
        fi
    fi

    if [ ! -f "$WALLPAPER_PATH" ]; then
        log_warn " Wallpaper not found: $WALLPAPER_PATH"
        return 1
    fi

    log_success "  Setting wallpaper: $WALLPAPER_PATH"

    # 获取当前的 monitor 配置（XFCE 可能使用不同的名称）
    # 动态获取可能会漏掉一些配置，所以最后会兜底设置常用路径
    local monitors=$(DISPLAY=:0 HOME=/home/user xfconf-query -c xfce4-desktop -l 2>/dev/null | grep 'workspace0/last-image' | head -10)

    if [ -n "$monitors" ]; then
        # 对于每个找到的 monitor 配置设置壁纸
        echo "$monitors" | while read monitor_path; do
            DISPLAY=:0 HOME=/home/user xfconf-query -c xfce4-desktop -p "$monitor_path" -s "$WALLPAPER_PATH" 2>/dev/null || true
            # 同时设置 image-style (5 = 缩放)
            local style_path=$(echo "$monitor_path" | sed 's/last-image/image-style/')
            DISPLAY=:0 HOME=/home/user xfconf-query -c xfce4-desktop -p "$style_path" -n -t int -s 5 2>/dev/null || true
        done
    fi

    # 兜底：确保所有常用的 monitor 路径都设置壁纸
    # 包括根级别的 monitor 配置（如 monitor0/monitor1）
    # 设置所有 workspace (0-3) 的壁纸

    # 1. 先设置根级别的 monitor 路径（这些优先级更高）
    for monitor_path in \
        "/backdrop/screen0/monitor0/last-image" \
        "/backdrop/screen0/monitor1/last-image"; do
        DISPLAY=:0 HOME=/home/user xfconf-query -c xfce4-desktop -p "$monitor_path" -n -t string -s "$WALLPAPER_PATH" 2>/dev/null || true
    done

    # 2. 设置所有 workspace 级别的路径
    for workspace in 0 1 2 3; do
        for monitor_path in \
            "/backdrop/screen0/monitorscreen/workspace${workspace}/last-image" \
            "/backdrop/screen0/monitor0/workspace${workspace}/last-image" \
            "/backdrop/screen0/monitor1/workspace${workspace}/last-image" \
            "/backdrop/screen0/monitorVNC-0/workspace${workspace}/last-image"; do
            DISPLAY=:0 HOME=/home/user xfconf-query -c xfce4-desktop -p "$monitor_path" -n -t string -s "$WALLPAPER_PATH" 2>/dev/null || true
        done
    done

    # 设置 image-path/image-show (某些 monitor 使用这种配置)
    for monitor_name in monitor0 monitor1; do
        DISPLAY=:0 HOME=/home/user xfconf-query -c xfce4-desktop -p "/backdrop/screen0/${monitor_name}/image-path" -n -t string -s "$WALLPAPER_PATH" 2>/dev/null || true
        DISPLAY=:0 HOME=/home/user xfconf-query -c xfce4-desktop -p "/backdrop/screen0/${monitor_name}/image-show" -n -t bool -s true 2>/dev/null || true
    done

    # 设置所有找到的 image-style
    for style_path in \
        "/backdrop/screen0/monitorscreen/workspace0/image-style" \
        "/backdrop/screen0/monitorVNC-0/workspace0/image-style" \
        "/backdrop/screen0/monitorVNC-0/workspace1/image-style" \
        "/backdrop/screen0/monitorVNC-0/workspace2/image-style" \
        "/backdrop/screen0/monitorVNC-0/workspace3/image-style"; do
        DISPLAY=:0 HOME=/home/user xfconf-query -c xfce4-desktop -p "$style_path" -n -t int -s 5 2>/dev/null || true
    done

    log_success "XFCE wallpaper config applied"

    # ========== 3. 等待壁纸实际渲染完成 ==========
    # xfdesktop 需要时间读取配置、加载图片、渲染到 X11 根窗口
    # 使用 xprop 检测桌面窗口的 _XROOTPMAP_ID 属性（表示背景图已设置）
    log "Waiting for wallpaper to render..."
    local render_counter=0
    local render_detected=false

    while ((render_counter < 16)); do  # 最长等待 8 秒
        # 检测根窗口是否已设置背景图 (xfdesktop 设置壁纸后会更新这个属性)
        if DISPLAY=:0 xprop -root _XROOTPMAP_ID 2>/dev/null | grep -q "pixmap id"; then
            render_detected=true
            log_success "Wallpaper rendering detected via _XROOTPMAP_ID"
            break
        fi

        # 备选检测：检查 xfdesktop 窗口是否存在并可见
        if DISPLAY=:0 xdotool search --class xfdesktop 2>/dev/null | head -1 | grep -q .; then
            # xfdesktop 窗口已存在，继续轮询 _XROOTPMAP_ID 最多 2 秒，确认壁纸已渲染
            local pixmap_wait=0
            while ((pixmap_wait < 4)); do
                if DISPLAY=:0 xprop -root _XROOTPMAP_ID 2>/dev/null | grep -q "pixmap id"; then
                    render_detected=true
                    log_success "Wallpaper rendering detected via _XROOTPMAP_ID (after xfdesktop window found)"
                    break 2  # 跳出两层循环
                fi
                sleep 0.5
                ((pixmap_wait++))
            done
            # 如果轮询后仍未检测到 pixmap，但 xfdesktop 窗口存在，也认为渲染完成
            render_detected=true
            log_success "Wallpaper rendering assumed complete (xfdesktop window exists)"
            break
        fi

        sleep 0.5
        ((render_counter++))
    done

    if [ "$render_detected" = false ]; then
        log_warn "Wallpaper render detection timed out, using fallback polling"
        # 降级方案：继续轮询 _XROOTPMAP_ID，每 0.5 秒检测一次，最多 3 秒
        local fallback_wait=0
        while ((fallback_wait < 6)); do
            if DISPLAY=:0 xprop -root _XROOTPMAP_ID 2>/dev/null | grep -q "pixmap id"; then
                log_success "Wallpaper rendering detected via fallback polling"
                break
            fi
            sleep 0.5
            ((fallback_wait++))
        done
    fi

    log_success "XFCE wallpaper applied and rendered successfully"

    # 🆕 写入壁纸就绪标记，供 VNC 就绪检查使用
    echo "$(date +%s)" > /tmp/wallpaper_ready
    log_success "Wallpaper ready marker written to /tmp/wallpaper_ready"
}

function check_vnc_health() {
    # 检查 VNC 服务健康状态 (as root)
    # 返回值：0=健康, 1=noVNC 异常(Xvnc 正常), 2=Xvnc 崩溃/卡死(需要重建整个显示栈)
    #
    # ⚠️ 禁止连 5900 端口做探测（wait_for_port 5900 / nc / RFB 半握手都不行）：
    #    TigerVNC 把任何不完整的 RFB 连接计为"安全失败"，累积后触发
    #    "Too many security failures" 锁定，拒绝前端正常连接（websockify 报 Target closed）。
    #    改用 xdpyinfo（X11 层）检测卡死——它走 X 协议、不碰 RFB，不会触发该锁定。
    #
    # 三关：
    #   ① Xvnc 进程存活且非僵尸(Z)/非卡死(D)
    #   ② X11 display 层验证（xdpyinfo，检测事件循环卡死）
    #   ③ noVNC 前端代理端口可达（6080，端口探测避免 WS 升级触发 websockify 连 5900）
    if [ "$VNC_AUTO_START" != "true" ]; then
        return 0
    fi

    # ① Xvnc 进程存活且非僵尸(Z)/非卡死(D 状态=不可中断睡眠)
    local xvnc_pid xvnc_stat
    xvnc_pid=$(pgrep -x "Xvnc" | head -1)
    if [ -z "$xvnc_pid" ]; then
        log_warn "check_vnc_health: no Xvnc process found"
        rm -f /tmp/vnc_ready
        return 2
    fi
    xvnc_stat=$(ps -o stat= -p "$xvnc_pid" 2>/dev/null)
    if [ -z "$xvnc_stat" ]; then
        log_warn "check_vnc_health: Xvnc pid $xvnc_pid disappeared (race)"
        rm -f /tmp/vnc_ready
        return 2
    fi
    case "$xvnc_stat" in
        Z*|*Z*)
            log_error "check_vnc_health: Xvnc pid $xvnc_pid is ZOMBIE (stat=$xvnc_stat)"
            rm -f /tmp/vnc_ready
            return 2
            ;;
        D*|*D*)
            log_error "check_vnc_health: Xvnc pid $xvnc_pid STUCK in D state (stat=$xvnc_stat)"
            rm -f /tmp/vnc_ready
            return 2
            ;;
    esac

    # ② X11 display 层验证（检测 Xvnc 事件循环卡死；走 X 协议，不碰 RFB/5900）
    if ! DISPLAY=:0 xdpyinfo >/dev/null 2>&1; then
        log_error "check_vnc_health: Xvnc pid $xvnc_pid X11 display frozen (xdpyinfo fails)"
        rm -f /tmp/vnc_ready
        return 2
    fi

    # ③ noVNC 前端代理端口可达（6080）。
    # ⚠️ 不做 WS 升级：websockify 是 WS↔TCP proxy，WS 升级会触发它连后端 5900，
    #    半握手被 TigerVNC 计为"安全失败"触发锁定。代理是否就绪由端口判断，
    #    Xvnc 后端服务可用性由 rcoder 代码侧完整 RFB 握手（check_vnc_rfb_ready）验证。
    if ! wait_for_port localhost 6080 3; then
        log_warn "check_vnc_health: Xvnc healthy but noVNC proxy port 6080 down"
        rm -f /tmp/vnc_ready
        return 1
    fi

    return 0
}

# 重建整个显示栈：Xvnc + D-Bus session + fcitx5 + xfdesktop + xfce4-session + noVNC
# 当 Xvnc 崩溃时调用，因为 Xvnc 死亡会导致所有 X11 客户端进程一起挂掉
function restart_full_display_stack() {
    log_error "Xvnc crashed, rebuilding full display stack..."

    # 1. 清除所有就绪标记
    rm -f /tmp/vnc_ready /tmp/novnc_port_ready /tmp/wallpaper_ready

    # 2. 清理残留进程（Xvnc 死后这些都已断连，但可能变僵尸）
    pkill -f "Xvnc :0" 2>/dev/null || true
    pkill -f "xfce4-session" 2>/dev/null || true
    pkill -f "xfdesktop" 2>/dev/null || true
    pkill -f "fcitx5" 2>/dev/null || true
    pkill -f "novnc_proxy" 2>/dev/null || true
    pkill -f "xfce4-panel" 2>/dev/null || true
    pkill -f "xfwm4" 2>/dev/null || true
    pkill -f "xfsettingsd" 2>/dev/null || true
    sleep 2

    # 3. 清理 X11 锁文件
    rm -f /tmp/.X0-lock /tmp/.X11-unix/X0

    # 4. 重启 Xvnc
    log "Restarting Xvnc :0 ..."
    HOME=/home/user XAUTHORITY=/tmp/.Xauthority MESA_SHADER_CACHE_DIR=/tmp/mesa_shader_cache \
        Xvnc :0 -geometry 1400x1050 -depth 24 -SecurityTypes None -ac -rfbport 5900 -FrameRate 20 -AlwaysShared \
        >/tmp/xvnc.log 2>&1 &

    # 5. 等待 X11 display 就绪
    local counter=0
    while ! DISPLAY=:0 xdpyinfo >/dev/null 2>&1; do
        sleep 0.2
        counter=$((counter + 1))
        if [ $counter -ge 50 ]; then
            log_error "Xvnc restart failed: X11 display not ready after 10s"
            return 1
        fi
    done
    log_success "Xvnc restarted, X11 display :0 ready"

    # 6. 设置背景色（避免纯黑）
    DISPLAY=:0 xsetroot -solid "#1e1e1e" 2>/dev/null || true

    # 7. 获取 D-Bus session 地址
    local DBUS_ADDR=""
    if [ -f /tmp/dbus-session-env ]; then
        source /tmp/dbus-session-env
        DBUS_ADDR="$DBUS_SESSION_BUS_ADDRESS"
    fi

    # 8. 重启 fcitx5 输入法
    env HOME=/home/user DISPLAY=:0 DBUS_SESSION_BUS_ADDRESS="${DBUS_ADDR}" \
        LANG=C.UTF-8 LC_ALL=C.UTF-8 LC_CTYPE=C.UTF-8 \
        GTK_IM_MODULE=fcitx QT_IM_MODULE=fcitx XMODIFIERS=@im=fcitx INPUT_METHOD=fcitx \
        fcitx5 -d --replace >/tmp/fcitx5-startup.log 2>&1 &

    # 9. 重启 xfdesktop（壁纸渲染）
    env DISPLAY=:0 HOME=/home/user XDG_CURRENT_DESKTOP=XFCE \
        DBUS_SESSION_BUS_ADDRESS="${DBUS_ADDR}" \
        xfdesktop &

    # 10. 重启 xfce4-session（桌面会话管理器）
    env DISPLAY=:0 HOME=/home/user XDG_CURRENT_DESKTOP=XFCE XDG_SESSION_DESKTOP=xfce \
        XDG_RUNTIME_DIR=/run/user/0 DBUS_SESSION_BUS_ADDRESS="${DBUS_ADDR}" \
        LANG=C.UTF-8 LC_ALL=C.UTF-8 LC_CTYPE=C.UTF-8 \
        GTK_IM_MODULE=fcitx QT_IM_MODULE=fcitx XMODIFIERS=@im=fcitx INPUT_METHOD=fcitx \
        xfce4-session &

    # 11. 等待 xfdesktop 启动
    if wait_for_process "xfdesktop" 10; then
        log_success "xfdesktop restarted"
    else
        log_warn "xfdesktop not detected after restart"
    fi

    # 12. 重启 noVNC 代理
    nohup /opt/noVNC/utils/novnc_proxy \
        --vnc localhost:5900 --listen 6080 --web /opt/noVNC \
        --heartbeat 30 \
        > /tmp/novnc.log 2>&1 &

    # 13. 验证 noVNC 端口 + WebSocket
    if wait_for_port localhost 6080 20 && wait_for_novnc_ready 10; then
        echo "$(date +%s)" > /tmp/novnc_port_ready
        log_success "noVNC proxy restarted and WebSocket verified"
    else
        log_error "noVNC restart failed after Xvnc recovery"
        return 1
    fi

    # 14. 重新应用壁纸（后台，不阻塞）
    (
        apply_xfce_wallpaper
    ) &

    # 15. 重启 VNC 就绪标记轮询（后台）
    # novnc_port_ready 已写入，等 wallpaper_ready + xfdesktop 进程就绪后写 vnc_ready
    (
        wait_and_write_vnc_ready_marker
    ) &

    log_success "Full display stack rebuilt successfully"
    return 0
}

function check_mcp_proxy_health() {
    # 检查 MCP Proxy 服务健康状态
    # MCP Proxy 运行在 127.0.0.1:18099，提供 chrome-devtools MCP 服务
    # 使用 mcp-proxy health 命令进行健康检查，支持 Streamable HTTP 协议

    local MCP_PROXY_PORT=18099

    # 1. 快速检查 mcp-proxy 进程是否存在
    if ! pgrep -f "mcp-proxy proxy" >/dev/null 2>&1; then
        log_warn "MCP Proxy process not running"
        return 1
    fi

    # 2. 使用 mcp-proxy health 命令检查服务健康状态
    #    -q: 静默模式，只返回退出码（0=健康，1=不健康）
    #    --timeout 5: 超时 5 秒
    if mcp-proxy health "http://127.0.0.1:${MCP_PROXY_PORT}" -q --timeout 5; then
        # 健康检查成功，不输出日志以减少日志量
        return 0
    else
        log_warn "MCP Proxy health check failed on port ${MCP_PROXY_PORT}"
        return 1
    fi
}

function restart_mcp_proxy() {
    # 重启 MCP Proxy 服务
    log "Restarting MCP Proxy service..."

    local MCP_LOG_DIR="${CONTAINER_LOGS_DIR:-/app/container-logs}/mcp"
    local MCP_CONFIG_FILE="/etc/mcp/mcp-proxy-config.json"

    # 确保日志目录存在
    mkdir -p "$MCP_LOG_DIR" 2>/dev/null || true

    # ========== 1. 停止现有 MCP Proxy 进程 ==========
    log "  Stopping existing MCP Proxy processes..."
    pkill -f "mcp-proxy proxy" || true
    sleep 1

    # 强制杀死残留进程
    pkill -9 -f "mcp-proxy proxy" 2>/dev/null || true
    sleep 1

    # ========== 2. 🔥 关键修复：清理所有 chrome-headless 子进程（解决僵尸进程问题）==========
    log "  Cleaning up chrome-headless and chromium processes..."

    # 2.1 清理 chrome-headless 进程（包括僵尸进程）
    local chrome_count=$(ps aux | grep -E '[c]hrome-headless' | wc -l)
    if [ "$chrome_count" -gt 0 ]; then
        log "  Found $chrome_count chrome-headless processes, terminating..."
        # 优雅终止
        pkill -TERM -f "chrome-headless" 2>/dev/null || true
        sleep 2
        # 强制杀死残留进程
        pkill -9 -f "chrome-headless" 2>/dev/null || true
    fi

    # 2.2 清理 chromium-for-mcp 进程
    local chromium_count=$(ps aux | grep -E '[c]hromium-for-mcp' | wc -l)
    if [ "$chromium_count" -gt 0 ]; then
        log "  Found $chromium_count chromium-for-mcp processes, terminating..."
        pkill -TERM -f "chromium-for-mcp" 2>/dev/null || true
        sleep 2
        pkill -9 -f "chromium-for-mcp" 2>/dev/null || true
    fi

    # 2.3 清理所有 chromium 相关的僵尸进程
    # 🔧 修复：使用更兼容的方式，避免 xargs -r
    local zombie_pids=$(ps aux | grep -E '[c]hrome.*<defunct>|[c]hromium.*<defunct>' | awk '{print $2}')
    if [ -n "$zombie_pids" ]; then
        echo "$zombie_pids" | xargs kill -9 2>/dev/null || true
    fi

    # 2.4 清理 chrome-devtools-mcp 进程
    pkill -9 -f "chrome-devtools-mcp" 2>/dev/null || true

    # 2.5 清理 Chromium 相关的锁文件和临时文件
    local CHROMIUM_DATA_DIR="${CHROMIUM_USER_DATA_DIR:-/home/user/.config/chromium}"
    if [ -d "$CHROMIUM_DATA_DIR" ]; then
        log "  Cleaning Chromium lock files in $CHROMIUM_DATA_DIR..."
        # 删除 SingletonLock 文件
        rm -f "$CHROMIUM_DATA_DIR/SingletonLock" 2>/dev/null || true
        rm -f "$CHROMIUM_DATA_DIR/SingletonSocket" 2>/dev/null || true
        rm -f "$CHROMIUM_DATA_DIR/SingletonCookie" 2>/dev/null || true
        # 清理所有锁文件
        find "$CHROMIUM_DATA_DIR" -name "*.lock" -type f -delete 2>/dev/null || true
        find "$CHROMIUM_DATA_DIR" -name "lockfile" -type f -delete 2>/dev/null || true
    fi

    # 2.6 清理 /tmp 中的 Chromium 临时文件
    rm -rf /tmp/.org.chromium.Chromium.* 2>/dev/null || true
    rm -rf /tmp/chrome_* 2>/dev/null || true

    # 2.7 清理 /dev/shm 中的 Chromium 共享内存
    rm -rf /dev/shm/.org.chromium.Chromium.* 2>/dev/null || true

    sleep 2

    # 2.8 统计清理后的进程数
    local remaining_chrome=$(ps aux | grep -E '[c]hrome-headless|[c]hromium-for-mcp' | grep -v grep | wc -l)
    if [ "$remaining_chrome" -gt 0 ]; then
        log_warn "  ⚠️  Still have $remaining_chrome chrome processes (may be from other containers)"
    else
        log_success "  ✅ All chrome-headless and chromium processes cleaned"
    fi

    # ========== 3. 检查配置文件 ==========
    if [ ! -f "$MCP_CONFIG_FILE" ]; then
        log_warn "MCP config file not found: $MCP_CONFIG_FILE, cannot restart"
        return 1
    fi

    # ========== 4. 获取 D-Bus 地址 ==========
    local DBUS_ADDR=""
    if [ -f /tmp/dbus-session-env ]; then
        source /tmp/dbus-session-env
        DBUS_ADDR="$DBUS_SESSION_BUS_ADDRESS"
    fi

    # ========== 5. 🔥 关键修复：使用进程组启动 mcp-proxy proxy 服务 ==========
    log "  Starting mcp-proxy proxy with process group management..."

    # 使用 setsid 创建新的会话和进程组，便于后续清理
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
        PATH="/usr/local/bin:/usr/local/cargo/bin:$PATH" \
        setsid bash -c "
            exec mcp-proxy proxy --port 18099 --host 127.0.0.1 --config-file '$MCP_CONFIG_FILE' --log-dir /app/container-logs -v \
            > '$MCP_LOG_DIR/mcp-proxy.log' 2>&1
        " &

    local MCP_PID=$!
    # 🔧 修复：使用更兼容的方式获取 PGID
    local MCP_PGID=$(ps -p $MCP_PID -o pgid= 2>/dev/null | tr -d '[:space:]')

    # 保存 PID 和 PGID 到文件，便于后续清理
    # 🔧 修复：添加错误处理，回退到持久化目录
    if ! echo "$MCP_PID" > /var/run/mcp-proxy.pid 2>/dev/null; then
        mkdir -p /app/container-logs
        echo "$MCP_PID" > /app/container-logs/mcp-proxy.pid
    fi

    if ! echo "$MCP_PGID" > /var/run/mcp-proxy.pgid 2>/dev/null; then
        mkdir -p /app/container-logs
        echo "$MCP_PGID" > /app/container-logs/mcp-proxy.pgid
    fi

    # ========== 6. 等待端口就绪（最长 15 秒）==========
    if wait_for_port 127.0.0.1 18099 15 && kill -0 $MCP_PID 2>/dev/null; then
        log_success "MCP Proxy restarted successfully (PID: $MCP_PID, PGID: $MCP_PGID)"
        return 0
    else
        log_warn "MCP Proxy restart failed, check log: $MCP_LOG_DIR/mcp-proxy.log"
        tail -20 "$MCP_LOG_DIR/mcp-proxy.log" 2>/dev/null || true
        return 1
    fi
}

# ============================================================================
# 🎯 VNC 就绪标记轮询任务
# 同时等待 noVNC 端口和桌面壁纸都就绪后，才写入最终的 VNC 就绪标记
# 这样前端打开 VNC 远程桌面时，桌面一定是正常显示的
# ============================================================================
function wait_and_write_vnc_ready_marker() {
    log "Starting VNC ready marker polling task (no timeout, will wait indefinitely)..."

    local interval=1    # 每秒检查一次
    local elapsed=0

    while true; do
        local novnc_ready=false
        local wallpaper_ready=false
        local desktop_ready=false

        # 检查 noVNC 端口就绪标记
        if [ -f /tmp/novnc_port_ready ]; then
            novnc_ready=true
        fi

        # 检查壁纸就绪标记
        if [ -f /tmp/wallpaper_ready ]; then
            wallpaper_ready=true
        fi

        # 检查 xfdesktop 桌面进程是否在运行
        # 确保桌面环境已加载，避免用户连上看到灰屏
        if pgrep -f "xfdesktop" >/dev/null 2>&1; then
            desktop_ready=true
        fi

        # 三者都就绪时，写入最终的 VNC 就绪标记
        if [ "$novnc_ready" = true ] && [ "$wallpaper_ready" = true ] && [ "$desktop_ready" = true ]; then
            echo "$(date +%s)" > /tmp/vnc_ready
            log_success "VNC ready marker written to /tmp/vnc_ready (noVNC + wallpaper + desktop all ready, took ${elapsed}s)"
            log_success "  noVNC port ready at: $(cat /tmp/novnc_port_ready)"
            log_success "  Wallpaper ready at: $(cat /tmp/wallpaper_ready)"
            return 0
        fi

        # 日志输出当前状态（每 30 秒输出一次，减少日志量）
        if ((elapsed % 30 == 0)) && ((elapsed > 0)); then
            log "Waiting for VNC ready... noVNC=$novnc_ready, wallpaper=$wallpaper_ready, desktop=$desktop_ready (${elapsed}s elapsed)"
        fi

        sleep $interval
        ((elapsed += interval))
    done
}

# Jupyter server function removed

# ============================================================================
# 🔊 音频流服务 (pcmflux)
# 使用 PulseAudio 虚拟声卡捕获音频，通过 WebSocket 流到浏览器
# ============================================================================
function start_audio_services() {
    log "Starting audio streaming services (pcmflux)..."

    # 1. 确保 PulseAudio 目录存在且有正确权限
    mkdir -p /home/user/.config/pulse
    mkdir -p /var/run/pulse
    chmod 755 /home/user/.config/pulse
    chmod 777 /var/run/pulse

    # 2. 创建 PulseAudio 客户端配置文件（允许 root 连接）
    cat > /home/user/.config/pulse/client.conf <<'EOF'
# 允许 root 用户连接
allow-pubkey-authentication=no
default-server=unix:/var/run/pulse/native
EOF

    # 3. 创建 PulseAudio 守护进程配置（禁用自动启动锁）
    cat > /home/user/.config/pulse/daemon.conf <<'EOF'
# 禁用自动启动锁
autospawn = no
exit-idle-time = -1
log-level = warning
EOF

    # 4. 修复 /home/user 目录权限（如果被挂载覆盖）
    chown -R user:user /home/user/.config/pulse 2>/dev/null || true

    # 5. 使用 --system 模式启动 PulseAudio（容器环境）
    # --disallow-exit: 防止 PulseAudio 自动退出
    # --disable-shm: 禁用共享内存（容器环境兼容性）
    log "  Starting PulseAudio in system mode..."
    pulseaudio --system \
        --disallow-exit \
        --disable-shm \
        --no-cpu-limit \
        --log-level=warning \
        --daemonize=no \
        2>/tmp/pulseaudio.log &

    # 等待 PulseAudio 进程启动
    if wait_for_process "pulseaudio" 5; then
        log_success "  PulseAudio daemon started (system mode)"
    else
        log_warn "  PulseAudio failed to start, checking log..."
        cat /tmp/pulseaudio.log 2>/dev/null || true
        return 1
    fi

    # 6. 等待 PulseAudio socket 就绪
    local pulse_socket="/var/run/pulse/native"
    local counter=0
    while [ ! -S "$pulse_socket" ]; do
        sleep 0.2
        let counter++
        if ((counter > 25)); then
            log_warn "  PulseAudio socket not ready: $pulse_socket"
            return 1
        fi
    done
    log_success "  PulseAudio socket ready: $pulse_socket"

    # 7. 设置 PULSE_SERVER 环境变量
    export PULSE_SERVER="unix:/var/run/pulse/native"
    echo "export PULSE_SERVER='unix:/var/run/pulse/native'" >> /etc/profile.d/pulse-env.sh

    # 8. 创建虚拟声卡
    log "  Creating virtual audio sink..."
    if pactl load-module module-null-sink sink_name=virtual_speaker \
          sink_properties=device.description="Virtual_Speaker" 2>/dev/null; then
        log_success "  Virtual speaker sink created"
    else
        log_warn "  Failed to create virtual speaker sink"
        return 1
    fi

    # 9. 设置虚拟声卡为默认输出
    pactl set-default-sink virtual_speaker 2>/dev/null || true

    # 10. 启动 pcmflux 音频流服务
    log "  Starting pcmflux audio streaming service..."
    export AUDIO_DEVICE="virtual_speaker.monitor"
    export AUDIO_HTTP_PORT=6090
    export AUDIO_WS_PORT=6089

    nohup python3 /usr/local/bin/audio_server.py > /tmp/audio_server.log 2>&1 &

    if wait_for_process_pattern "audio_server.py" 3 && wait_for_port localhost 6090 3; then
        log_success "  pcmflux audio server started"
        log_success "  Audio HTTP: http://localhost:6090"
        log_success "  Audio WebSocket: ws://localhost:6089"
    else
        log_warn "  pcmflux audio server failed to start"
        cat /tmp/audio_server.log 2>/dev/null | tail -20 || true
        return 1
    fi

    log_success "Audio streaming services initialized"
    return 0
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

    # ========== 🔥 关键修复：使用进程组启动 mcp-proxy proxy 服务 ==========
    # 需要传递正确的环境变量（DISPLAY, D-Bus, 输入法等）
    echo "  Starting mcp-proxy proxy on port 18099 (with process group)..."

    # 使用 setsid 创建新的会话和进程组，便于后续清理所有子进程
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
        PATH="/usr/local/bin:/usr/local/cargo/bin:$PATH" \
        setsid bash -c "
            exec mcp-proxy proxy --port 18099 --host 127.0.0.1 --config-file '$MCP_CONFIG_FILE' --log-dir /app/container-logs -v \
            > '$MCP_LOG_DIR/mcp-proxy.log' 2>&1
        " &

    local MCP_PID=$!
    # 🔧 修复：使用更兼容的方式获取 PGID
    local MCP_PGID=$(ps -p $MCP_PID -o pgid= 2>/dev/null | tr -d '[:space:]')

    # 保存 PID 和 PGID 到文件，便于后续清理
    # 🔧 修复：添加错误处理，回退到持久化目录
    if ! echo "$MCP_PID" > /var/run/mcp-proxy.pid 2>/dev/null; then
        mkdir -p /app/container-logs
        echo "$MCP_PID" > /app/container-logs/mcp-proxy.pid
    fi

    if ! echo "$MCP_PGID" > /var/run/mcp-proxy.pgid 2>/dev/null; then
        mkdir -p /app/container-logs
        echo "$MCP_PGID" > /app/container-logs/mcp-proxy.pgid
    fi

    # 等待 MCP Proxy 端口就绪（智能等待，最长 10 秒）
    if wait_for_port 127.0.0.1 18099 10 && kill -0 $MCP_PID 2>/dev/null; then
        log_success "  MCP Proxy started (PID: $MCP_PID, PGID: $MCP_PGID)"
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

# ============================================================================
# 🐘 PostgreSQL + pgweb（开发环境本地数据库 + Web UI）
# 用户在 agent-runner 容器开发时用本地 PG 测试；开发完打包发布到 UserApp。
# PG 数据落 /home/user/.pgdata（持久化，容器重启保留）；pgweb Web UI 操作数据库。
#
# ⚠️ 启动模型: 此处只做"非阻塞"准备（备 PGDATA 归属 + 写连接信息）；首次 initdb
# 与 postgres 进程均由 supervisor 托管的 pg-supervisor-entry.sh 异步拉起。
# agent_runner 不依赖 PG（PG 仅供用户开发用），故 PG initdb 再慢也不阻塞 :8086
# health —— 旧版在此同步 initdb, CephFS 上 >60s, 导致 agent_runner 被 liveness 杀。
# ============================================================================
function prepare_pg() {
    export PGDATA="${PGDATA:-/home/user/.pgdata}"
    export POSTGRES_USER="${POSTGRES_USER:-dev}"
    export POSTGRES_PASSWORD="${POSTGRES_PASSWORD:-dev}"
    export POSTGRES_DB="${POSTGRES_DB:-dev}"
    export PGWEB_PORT="${PGWEB_PORT:-8081}"

    mkdir -p /app/logs
    # 仅备好 PGDATA 目录归属（瞬时: 无 fsync、无递归 chown）。
    # 只 chown .pgdata 本身，不碰 /home/user —— 旧版 `chown -R $(dirname $PGDATA)`
    # 会递归 chown 整个用户主目录（= /home/user），既慢（CephFS 递归）又会把用户
    # 项目文件属主错改成 postgres。
    install -d -o postgres -g postgres "$PGDATA"

    # 连接信息（供用户参考；postgres/pgweb 进程由 supervisor 管）
    cat > /home/user/pg-connection.txt <<EOF
PostgreSQL 开发数据库:
  容器内: host=localhost port=5432 user=$POSTGRES_USER password=$POSTGRES_PASSWORD database=$POSTGRES_DB sslmode=disable
pgweb: http://localhost:$PGWEB_PORT  (Add Connection 填上述信息)
EOF
    log "PG prepared (PGDATA=$PGDATA); initdb/postgres 由 supervisor 异步拉起"
}

# ============================================================================
# 🖥️ ttyd Web 终端 —— 由 supervisor 管（conf.d/ttyd.conf，autorestart 自动拉起）
# 不再在此 nohup 启动（阶段1 迁 supervisor；进程崩溃 supervisor 自动重启）
# ============================================================================

# 设置VNC自动启动标志
export VNC_AUTO_START=true

# ========== 关键：在启动 X11 之前初始化用户主目录 ==========
# 从骨架目录恢复配置（解决挂载空目录导致的花屏和图标消失）
initialize_user_home

# ========== supervisor 管 ttyd/PG/pgweb（不依赖 X11，初始化后启动）==========
# 阶段1：ttyd/PG/pgweb 迁 supervisor（autorestart 崩溃自动拉起）
# VNC/XFCE/MCP/audio/IME 仍由下方 start-up.sh 管（保留自定义）
prepare_pg                                       # PG 非阻塞准备（initdb 已异步进 supervisor）
supervisord -c /etc/supervisor/supervisord.conf  # daemon，autostart postgres/pgweb/ttyd
log "supervisord started (manages: postgresql, pgweb, ttyd — autorestart on crash)"

# ========== MCP Proxy 服务在 X11 就绪后启动 ==========
# 注意：chrome-devtools-mcp 需要 X11 来启动 Chromium 浏览器
# 因此必须等待 Xvnc 启动后才能启动 MCP Proxy
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
        if start_vnc_services; then
            log_success "VNC services started successfully!"
            log_success "VNC URL: http://localhost:6080/vnc.html?autoconnect=true&resize=scale"
            log_success "Direct VNC port: 5900"
            # 桌面图标信任化: 兜底保证文件属性(user属主+755); 骨架恢复 cp -aL 已做过, 这里后台二次确认, 不阻塞主流程
            ( sleep 3 && trust_desktop_icons ) &
        else
            log_error "VNC services failed to start, /tmp/novnc_port_ready will NOT be written"
        fi
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

    # 4.5 ttyd + PostgreSQL + pgweb —— 由 supervisor 管（autorestart），不在此启动
    # （见上方 supervisord 启动；阶段1 迁 supervisor）

    # 5. 应用 XFCE 壁纸（后台）
    (
        apply_xfce_wallpaper
    ) &
    wallpaper_pid=$!

    # 等待所有并行服务启动完成
    log "Waiting for all services to start..."
    wait $vnc_pid $mcp_pid $audio_pid $ime_pid $wallpaper_pid 2>/dev/null || true
    log_success "All X11-dependent services started!"

    # 🆕 启动 VNC 就绪标记轮询任务（后台）
    # 等待 noVNC 端口和壁纸都就绪后，才写入最终的 /tmp/vnc_ready 标记
    (
        wait_and_write_vnc_ready_marker
    ) &
    vnc_ready_marker_pid=$!
    log "VNC ready marker polling task started (pid: $vnc_ready_marker_pid)"

    # VNC 和 MCP Proxy 服务监控循环 (as root)
    while true; do
        sleep 30

        # 检查 VNC 服务健康状态
        # 返回值: 0=健康, 1=noVNC 异常, 2=Xvnc 崩溃
        # 注意：此处不能用 local，因为当前在子 shell 中而非函数内
        check_vnc_health
        vnc_status=$?

        if [ $vnc_status -eq 2 ]; then
            # Xvnc 崩溃：整个显示栈需要重建
            # Xvnc 是 X11 server + VNC server，它死了 = display :0 消失
            # 所有连接到 :0 的进程（xfdesktop, xfce4-session, fcitx5 等）全部跟着死
            # 只重启 noVNC 没用，因为 noVNC 连不上已死的 5900 端口
            restart_full_display_stack || log_error "Full display stack restart failed"

        elif [ $vnc_status -eq 1 ]; then
            # 仅 noVNC 代理异常，Xvnc 仍在运行
            log_warn "noVNC proxy is down, restarting..."
            rm -f /tmp/vnc_ready /tmp/novnc_port_ready

            pkill -f novnc_proxy || true
            sleep 1

            nohup /opt/noVNC/utils/novnc_proxy \
                --vnc localhost:5900 \
                --listen 6080 \
                --web /opt/noVNC \
                --heartbeat 30 \
                > /tmp/novnc.log 2>&1 &

            # 等待端口 + WebSocket 就绪后，重新写入标记
            if wait_for_port localhost 6080 20 && wait_for_novnc_ready 10; then
                echo "$(date +%s)" > /tmp/novnc_port_ready
                # Xvnc 仍在运行，桌面进程应该也还在
                if [ -f /tmp/wallpaper_ready ] && pgrep -f "xfdesktop" >/dev/null 2>&1; then
                    echo "$(date +%s)" > /tmp/vnc_ready
                    log_success "noVNC proxy restarted and verified successfully"
                else
                    log_warn "noVNC restarted but desktop not ready, waiting for marker task"
                fi
            else
                log_error "noVNC restart failed: WebSocket not ready"
            fi
        fi

        # ========== MCP Proxy 健康检查（使用 mcp-proxy health 工具）==========
        # 使用 mcp-proxy health 命令进行健康检查，正确处理 Streamable HTTP 协议
        if ! check_mcp_proxy_health; then
            echo "MCP Proxy 服务异常，正在重启..."
            if ! restart_mcp_proxy; then
                # 首次重启失败，等待 5 秒后重试一次
                sleep 5
                restart_mcp_proxy || log_warn "MCP Proxy restart failed after retry"
            fi
        fi

        # 注:孤儿/僵尸进程回收 + 信号转发由 agent_runner 内置的 pid1 库负责 —— start-up.sh
        # exec 进 agent_runner 后,若它是 PID 1,pid1 会 re-exec 自己为子进程 + 本进程做监督
        # (进程外 waitpid,不与 tokio::process 冲突)。旧 in-process process_reaper 模块已删。
    done
) &

# 启动 agent_runner 服务，支持从环境变量读取端口
log "Starting agent_runner service on port ${PORT:-8086}..."

# 🔧 如果启用 eBPF 调试模式，启动自动火焰图生成
if [ "${ENABLE_EBPF_AUTO_FLAMEGRAPH:-false}" = "true" ]; then
    log "🔧 启动 eBPF 自动火焰图生成..."
    /usr/local/bin/ebpf-tools/auto-flamegraph.sh start &
    AUTO_FLAME_PID=$!
    log "✅ eBPF 自动火焰图生成已启动 (PID: $AUTO_FLAME_PID)"
    log "📋 火焰图将每 ${GENERATE_INTERVAL:-60} 秒生成一次"
    log "💡 火焰图保存到: ${DIAG_OUTPUT_DIR:-/app/container-logs/diag}/flamegraph-*.svg"
fi

# ============================================================================
# 📊 Grafana Alloy - 持续性能数据采集（替代已废弃的 Pyroscope Agent）
# 默认禁用，通过环境变量 ENABLE_ALLOY=true 启用
# ============================================================================
if [ "${ENABLE_ALLOY:-false}" = "true" ]; then
    log "🔧 启动 Grafana Alloy (eBPF Profiling)..."

    # Pyroscope Server 地址
    PYROSCOPE_URL="${PYROSCOPE_URL:-http://pyroscope:4040}"

    # 设置环境变量（用于 Alloy 配置文件）
    export PYROSCOPE_URL
    export PROJECT_ID="${PROJECT_ID:-default}"
    export ENV="${ENV:-dev}"
    export HOSTNAME="${HOSTNAME:-$(hostname)}"

    # 显示环境变量（用于调试）
    log "  环境变量:"
    log "    PYROSCOPE_URL=$PYROSCOPE_URL"
    log "    PROJECT_ID=$PROJECT_ID"
    log "    ENV=$ENV"
    log "    HOSTNAME=$HOSTNAME"

    # 等待 Pyroscope Server 就绪
    log "  等待 Pyroscope Server 就绪: $PYROSCOPE_URL"
    local pyro_ready=false
    local counter=0
    while [ $counter -lt 60 ]; do
        if curl -s -o /dev/null -w "%{http_code}" "$PYROSCOPE_URL" 2>/dev/null | grep -q "200\|404"; then
            pyro_ready=true
            break
        fi
        sleep 0.5
        counter=$((counter + 1))
    done

    if [ "$pyro_ready" = false ]; then
        log_warn "  Pyroscope Server 未就绪，跳过 Alloy 启动"
    else
        log_success "  Pyroscope Server 已就绪"

        # 创建日志目录
        mkdir -p "${CONTAINER_LOGS_DIR:-/app/container-logs}/diag"

        # 检查 Alloy 是否安装
        if ! command -v alloy >/dev/null 2>&1; then
            log_warn "  Alloy 未安装，跳过启动"
        else
            # 验证 Alloy 配置文件
            log "  验证 Alloy 配置文件..."
            if alloy validate /etc/alloy/config.alloy 2>&1 | tee -a "${CONTAINER_LOGS_DIR:-/app/container-logs}/diag/alloy.log"; then
                log_success "  Alloy 配置文件验证通过"
            else
                log_warn "  Alloy 配置文件验证失败，但仍将尝试启动"
            fi

            # 后台启动 Grafana Alloy
            # 注意：eBPF 需要 root 权限，容器已通过 privileged 运行
            log "  启动 Alloy 进程..."
            nohup alloy run /etc/alloy/config.alloy \
                > "${CONTAINER_LOGS_DIR:-/app/container-logs}/diag/alloy.log" 2>&1 &

            local alloy_pid=$!
            log_success "  Grafana Alloy 已启动 (PID: $alloy_pid)"
            log "  📊 性能数据发送到: $PYROSCOPE_URL"
            log "  💡 Web UI: http://localhost:4040"
            log "  🔍 监控进程: agent_runner 及其子进程"
            log "  📝 日志文件: ${CONTAINER_LOGS_DIR:-/app/container-logs}/diag/alloy.log"

            # 等待 3 秒后检查 Alloy 进程状态
            sleep 3
            if ps -p "$alloy_pid" > /dev/null 2>&1; then
                log_success "  Alloy 进程运行中"
                # 显示最近的日志（用于调试）
                log "  最近的 Alloy 日志:"
                tail -n 5 "${CONTAINER_LOGS_DIR:-/app/container-logs}/diag/alloy.log" 2>/dev/null | while IFS= read -r line; do
                    log "    $line"
                done
            else
                log_warn "  Alloy 进程已退出，请检查日志"
                log "  错误日志:"
                tail -n 10 "${CONTAINER_LOGS_DIR:-/app/container-logs}/diag/alloy.log" 2>/dev/null | while IFS= read -r line; do
                    log "    $line"
                done
            fi
        fi
    fi
else
    log "📊 Grafana Alloy 已禁用 (设置 ENABLE_ALLOY=true 启用)"
fi

# ============================================================================
# 🔍 Off-CPU 阻塞监控 (offcputime-bpfcc)
# ============================================================================
if [ "${ENABLE_OFFCPUTIME:-false}" = "true" ]; then
    log "🔧 启动 Off-CPU 阻塞监控..."

    # 检查 offcputime-bpfcc 是否可用
    if ! command -v offcputime-bpfcc &> /dev/null; then
        log_warn "  offcputime-bpfcc 未安装，跳过 Off-CPU 监控"
    else
        # 创建 offcpu-monitor.sh 脚本（如果不存在）
        if [ ! -f "/usr/local/bin/ebpf-tools/offcpu-monitor.sh" ]; then
            log_warn "  offcpu-monitor.sh 未找到，跳过 Off-CPU 监控"
        else
            # 启动 Off-CPU 监控
            /usr/local/bin/ebpf-tools/offcpu-monitor.sh start &
            local offcpu_monitor_pid=$!
            log_success "  Off-CPU 监控已启动 (PID: $offcpu_monitor_pid)"
            log "  📊 阻塞火焰图将每 ${OFFCPU_INTERVAL:-300} 秒生成一次"
            log "  💡 阻塞火焰图保存到: ${DIAG_OUTPUT_DIR:-/app/container-logs/diag}/offcpu-*.svg"
        fi
    fi
fi

# ============================================================================
# 🔍 系统调用监控 (syscalls)
# ============================================================================
if [ "${ENABLE_SYSCALL_MONITOR:-false}" = "true" ]; then
    log "🔧 启动系统调用监控..."

    # 检查 syscount-bpfcc 是否可用
    if ! command -v syscount-bpfcc &> /dev/null; then
        log_warn "  syscount-bpfcc 未安装，跳过系统调用监控"
    else
        # 启动系统调用监控
        /usr/local/bin/ebpf-tools/syscall-monitor.sh start &
        local syscall_monitor_pid=$!
        log_success "  系统调用监控已启动 (PID: $syscall_monitor_pid)"
        log "  📊 系统调用统计将每 ${GENERATE_INTERVAL:-60} 秒生成一次"
        log "  📝 日志文件: ${DIAG_OUTPUT_DIR:-/app/container-logs/diag}/syscall-monitor.log"
        log "  💡 统计结果保存到: ${DIAG_OUTPUT_DIR:-/app/container-logs/diag}/syscall-count-*.txt"
    fi
fi

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


# ========== MCP Proxy：不在 readiness 关键路径，不阻塞 agent_runner 启动 ==========
# MCP Proxy 仍在下方后台子 shell (X11 就绪后启动, ~line 1867) 里拉起，这里不再 wait_for_port。
# 原因：agent_runner 的 /health 只查 HTTP+gRPC:50051，不依赖 MCP；而 wait_for_port 18099
# 实际要等 "X11 就绪 + MCP 起来"，会把 exec agent_runner 拖后数秒 → restart create 阶段变慢。
# MCP 未就绪时由 agent_runner 内部重试逻辑兜底（用户首次调 agent 通常已在重启数秒后，MCP 已就绪）。

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
export RCODER_EMBED_FILE_SERVER=true; \
export PROJECT_SOURCE_DIR=/home/user; \
export USERAPP_WORKSPACE_DIR=/home/user/userapp-workspace; \
export FILE_SERVER_PORT=60000; \
export PATH=/usr/local/bin:/usr/local/cargo/bin:\$PATH"

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

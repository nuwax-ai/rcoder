# 独立构建 Dockerfile - 运行环境
# agent_runner 二进制文件通过 Dockerfile.build 在 debian:12 环境中编译
# 确保 GLIBC 版本与此镜像一致（都是 debian:12 / GLIBC 2.36）

FROM debian:12

# Base image with desktop environment and applications
# Environment variables:

ENV \
    # Avoid system prompts: \
    DEBIAN_FRONTEND=noninteractive \
    DEBIAN_PRIORITY=high \
    # Timezone: \
    TZ=Asia/Shanghai \
    # Pip settings: \
    PIP_DEFAULT_TIMEOUT=100 \
    PIP_DISABLE_PIP_VERSION_CHECK=1 \
    PIP_NO_CACHE_DIR=1

# Desktop environment and applications (merged for smaller image size):

RUN apt-get update && \
    # Install all packages in one layer to reduce image size
    apt-get install -y \
    # X window server:
    xserver-xorg xorg x11-xserver-utils xvfb x11-utils xauth \
    # XFCE desktop environment:
    xfce4 xfce4-goodies \
    # 中文输入法（fcitx5 完整安装）
    fcitx5 fcitx5-pinyin fcitx5-frontend-gtk3 fcitx5-frontend-gtk2 fcitx5-frontend-qt5 fcitx5-module-xorg fcitx5-config-qt im-config dbus-x11 \
    # Basic system utilities:
    util-linux sudo curl git \
    # Python pip:
    python3-pip \
    # Desktop SDK tools:
    xdotool scrot ffmpeg \
    # VNC and streaming:
    x11vnc net-tools netcat-openbsd \
    # Audio streaming (PulseAudio + pcmflux dependencies):
    pulseaudio pulseaudio-utils libpulse-dev libopus-dev \
    # Standard applications:
    x11-apps xpdf gedit xpaint tint2 galculator pcmanfm \
    # Dependencies for later installations:
    apt-transport-https ca-certificates gnupg \
    socat supervisor \
    xclip \
    wget software-properties-common \
    # Suna dependencies:
    poppler-utils wkhtmltopdf antiword unrtf catdoc gawk file jq csvkit xmlstarlet zip unzip tmux vim tree rsync && \
    # Configure X11 wrapper to allow any user:
    sed -i 's/allowed_users=console/allowed_users=anybody/' /etc/X11/Xwrapper.config && \
    # Set the default terminal
    ln -sf /usr/bin/xfce4-terminal.wrapper /etc/alternatives/x-terminal-emulator && \
    # Install Python packages:
    # apt-get install -y python3-numpy && \
    # Clone noVNC from nuwax-ai fork (based on official v1.6.0, with customized vnc.html):
    git clone -b release https://github.com/nuwax-ai/noVNC.git /opt/noVNC && \
    cd /opt/noVNC && \
    ln -s /opt/noVNC/vnc.html /opt/noVNC/index.html && \
    git clone -b v0.12.0 https://github.com/novnc/websockify /opt/noVNC/utils/websockify && \
    # Clean up package cache and temporary files to reduce image size:
    apt-get autoremove -y && \
    apt-get autoclean && \
    rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/* /root/.cache

# Install LibreOffice from official website
ARG TARGETARCH
RUN case "$TARGETARCH" in \
    "amd64") LO_ARCH="x86_64" ;; \
    "arm64") LO_ARCH="aarch64" ;; \
    *) echo "Unsupported architecture: $TARGETARCH" && exit 1 ;; \
    esac && \
    case "$TARGETARCH" in \
    "amd64") LO_ARCH2="x86-64" ;; \
    "arm64") LO_ARCH2="aarch64" ;; \
    *) echo "Unsupported architecture: $TARGETARCH" && exit 1 ;; \
    esac && \
    wget -O /tmp/libreoffice.tar.gz "https://mirrors.ustc.edu.cn/tdf/libreoffice/stable/25.8.4/deb/${LO_ARCH}/LibreOffice_25.8.4_Linux_${LO_ARCH2}_deb.tar.gz" && \
    cd /tmp && \
    tar -xzf libreoffice.tar.gz && \
    cd LibreOffice_*/DEBS && \
    dpkg -i *.deb || true && \
    apt-get update && \
    apt-get install -f -y && \
    # Clean up
    rm -rf /tmp/LibreOffice_* /tmp/libreoffice.tar.gz && \
    apt-get autoremove -y && \
    apt-get autoclean && \
    rm -rf /var/lib/apt/lists/*


# User applications:

# ~ Make your changes to this template BELOW this line ~

# Install Chromium from Debian official repository (optimized single layer)
RUN apt-get update && \
    apt-get install -y chromium chromium-driver && \
    # Clean up package cache to reduce image size:
    apt-get autoremove -y && \
    apt-get autoclean && \
    rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*

# Configure Chromium to skip first-run experience and enable CDP
RUN mkdir -p /etc/chromium/policies/managed/ && \
    echo '{"CommandLineFlagSecurityWarningsEnabled": false, "AllowInsecureLocalConnections": true, "RemoteDebuggingAllowed": true, "RemoteDebuggingPort": "9222", "RemoteDebuggingAddress":"0.0.0.0", "AutoplayAllowed": true, "MetricsReportingEnabled": false, "BrowserSignin": 0, "SyncDisabled": true, "PromotionalTabsEnabled": false, "DefaultBrowserSettingEnabled": false, "DefaultSearchProviderEnabled": false, "HideWebStoreIcon": true, "PasswordManagerEnabled": false, "PromptForDownloadLocation": false, "SafeBrowsingEnabled": false, "SafeBrowsingExtendedReportingEnabled": false, "UrlKeyedAnonymizedDataCollectionEnabled": false, "CloudPrintSubmitEnabled": false, "CloudManagementEnrollmentToken": "", "SpellcheckEnabled": false}' > /etc/chromium/policies/managed/policies.json

# Create user and configure Chrome (merged for smaller image size)
RUN apt-get update && \
    apt-get install -y gnome-keyring policykit-1-gnome policykit-1 && \
    # Create default user
    useradd -ms /bin/bash user && \
    usermod -aG sudo user && \
    passwd -d user && \
    echo "user ALL=(ALL:ALL) NOPASSWD: ALL" >>/etc/sudoers && \
    # Create Chromium configuration directories
    mkdir -p /home/user/.config/chromium/Default/ && \
    echo '{  "browser": { "custom_chrome_frame": true, "has_seen_welcome_page": true }, "profile": { "default_content_setting_values": { "geolocation": 1 }, "exit_type": "Normal", "exited_cleanly": true, "password_manager_enabled": false }, "credentials_enable_service": false, "credentials_enable_autosignin": false }' > /home/user/.config/chromium/Default/Preferences && \
    # Configure GNOME keyring with empty password (禁用密码提示)
    mkdir -p /home/user/.local/share/keyrings && \
    # 创建 Default keyring（无密码，禁用锁定）
    printf '[keyring]\ndisplay-name=Default keyring\nctime=1732780800\nmtime=1732780800\nlock-on-idle=false\nlock-after=false\n' > /home/user/.local/share/keyrings/Default_keyring.keyring && \
    # 设置为默认 keyring
    echo 'Default_keyring' > /home/user/.local/share/keyrings/default && \
    chmod 600 /home/user/.local/share/keyrings/Default_keyring.keyring && \
    chmod 600 /home/user/.local/share/keyrings/default && \
    # Configure PolicyKit to auto-allow Chromium operations
    mkdir -p /etc/polkit-1/localauthority/50-local.d && \
    # Create comprehensive policy for user
    printf '[Allow all for user]\nIdentity=unix-user:user\nAction=*\nResultAny=yes\nResultInactive=yes\nResultActive=yes\n\n[Allow all for user - specific patterns]\nIdentity=unix-user:user\nAction=org.freedesktop.policykit.*\nResultAny=yes\nResultInactive=yes\nResultActive=yes\n\n[Allow Chromium specific]\nIdentity=unix-user:user\nAction=org.chromium.*\nResultAny=yes\nResultInactive=yes\nResultActive=yes' > /etc/polkit-1/localauthority/50-local.d/comprehensive-allow.pkla && \
    # ========== 系统级自启动配置 (不受用户目录挂载影响) ==========
    mkdir -p /etc/xdg/autostart && \
    # PolicyKit authentication agent autostart
    printf "[Desktop Entry]\nType=Application\nName=PolicyKit Auth\nExec=/usr/lib/policykit-1-gnome/polkit-gnome-authentication-agent-1\nHidden=false\nNoDisplay=false\nX-GNOME-Autostart-enabled=true\nStartupNotify=false" > /etc/xdg/autostart/polkit-gnome-auth.desktop && \
    # D-Bus session autostart
    printf "[Desktop Entry]\nType=Application\nName=D-Bus Session\nExec=/usr/bin/dbus-launch --sh-syntax\nHidden=false\nNoDisplay=false\nX-GNOME-Autostart-enabled=true" > /etc/xdg/autostart/dbus-session.desktop && \
    # PolicyKit daemon autostart
    printf "[Desktop Entry]\nType=Application\nName=PolicyKit Daemon\nExec=/usr/lib/policykit-1/polkitd --no-debug\nHidden=false\nNoDisplay=false\nX-GNOME-Autostart-enabled=true" > /etc/xdg/autostart/polkitd.desktop && \
    # Fcitx5 autostart
    printf '[Desktop Entry]\nType=Application\nName=Fcitx 5\nComment=Input Method Framework\nExec=fcitx5 -d --replace\nIcon=fcitx5\nStartupNotify=false\nTerminal=false\nCategories=System;Utility;\nX-GNOME-Autostart-enabled=true\nX-XFCE-Autostart-enabled=true' > /etc/xdg/autostart/fcitx5.desktop && \
    # 创建用户运行时目录
    mkdir -p /run/user/$(id -u user) && \
    # ========== 系统级 fcitx5 输入法配置 ==========
    mkdir -p /etc/xdg/fcitx5/conf && \
    # fcitx5 全局配置
    printf '[Hotkey]\nEnumerateWithTriggerKeys=True\nAltTriggerKeys=\nEnumerateForwardKeys=\nEnumerateBackwardKeys=\nEnumerateSkipFirst=False\n\n[Hotkey/TriggerKeys]\n0=Control+space\n1=Shift+Control+space\n\n[Hotkey/EnumerateGroupForwardKeys]\n0=Super+space\n\n[Hotkey/EnumerateGroupBackwardKeys]\n0=Shift+Super+space\n\n[Hotkey/ActivateKeys]\n0=Hangul_Hanja\n\n[Hotkey/DeactivateKeys]\n0=Hangul_Romaja\n\n[Hotkey/PrevPage]\n0=Up\n\n[Hotkey/NextPage]\n0=Down\n\n[Hotkey/PrevCandidate]\n0=Shift+Tab\n\n[Hotkey/NextCandidate]\n0=Tab\n\n[Hotkey/TogglePreedit]\n0=Control+Alt+P\n\n[Behavior]\nActiveByDefault=True\nShareInputState=No\nPreloadInputMethod=True\nShowInputMethodInformation=True\nShowInputMethodInformationWhenFocusIn=False\nCompactInputMethodInformation=True\nShowFirstInputMethodInformation=True\nDefaultPageSize=5\nOverrideXkbOption=False\nCustomXkbOption=\nEnabledAddons=\nDisabledAddons=\nPreeditEnabledByDefault=True' > /etc/xdg/fcitx5/config && \
    # 拼音配置
    printf '[Addon]\nEnabled=True\n\n[Behavior]\nPreeditInApplication=False\n\n[PinyinEngine]\nShuangpinProfile=Ziranma\nShowActualPinyinInHint=False\nPredictionEnabled=True\nPredictionSize=10\nCloudPinyinEnabled=False\nCloudPinyinIndex=2' > /etc/xdg/fcitx5/conf/pinyin.conf && \
    # 禁用云拼音提示
    printf '[Addon]\nEnabled=False\n\n[Behavior]\nShowNotification=False' > /etc/xdg/fcitx5/conf/cloudpinyin.conf && \
    # 禁用通知
    printf '[Addon]\nEnabled=False\n\n[Behavior]\nHiddenNotifications=' > /etc/xdg/fcitx5/conf/notifications.conf && \
    # fcitx5 profile (系统默认)
    printf '[Groups/0]\nName=Default\nDefault Layout=us\nDefaultIM=pinyin\n\n[Groups/0/Items/0]\nName=keyboard-us\nLayout=\n\n[Groups/0/Items/1]\nName=pinyin\nLayout=\n\n[GroupOrder]\n0=Default\n\n[Behavior]\nActiveByDefault=True\nShareInputState=No' > /etc/xdg/fcitx5/profile && \
    # ========== 系统级环境变量配置 (替代 .bashrc/.xprofile) ==========
    mkdir -p /etc/profile.d && \
    printf '#!/bin/sh\n# Fcitx5 Input Method Environment Variables\nexport GTK_IM_MODULE=fcitx5\nexport QT_IM_MODULE=fcitx5\nexport XMODIFIERS=@im=fcitx5\nexport INPUT_METHOD=fcitx5\nexport SDL_IM_MODULE=fcitx5\nexport GLFW_IM_MODULE=fcitx5\n' > /etc/profile.d/fcitx5-env.sh && \
    chmod +x /etc/profile.d/fcitx5-env.sh && \
    # ========== 系统级 GTK 配置 ==========
    mkdir -p /etc/gtk-3.0 && \
    printf '[Settings]\ngtk-im-module=fcitx5' > /etc/gtk-3.0/settings.ini && \
    # ========== 隐藏 root 用户警告 ==========
    # 为 root 用户创建 GTK CSS 配置，彻底隐藏 Thunar 和 XFCE 的各种 root 警告
    mkdir -p /root/.config/gtk-3.0 && \
    printf '/* Hide Thunar root warnings completely */\n.thunar-window infobar.warning { min-height: 0; max-height: 0; padding: 0; margin: 0; opacity: 0; }\n.thunar-window infobar.warning * { min-height: 0; max-height: 0; padding: 0; margin: 0; opacity: 0; }\n.thunar-window infobar.warning button { min-height: 0; min-width: 0; max-height: 0; padding: 0; margin: 0; opacity: 0; }\ninfobar.warning { min-height: 0; max-height: 0; padding: 0; margin: 0; opacity: 0; }\ninfobar.warning * { min-height: 0; max-height: 0; padding: 0; margin: 0; opacity: 0; }\n/* Hide XFCE root warning */\n.root-warning { display: none !important; opacity: 0 !important; }\n.root-warning * { display: none !important; opacity: 0 !important; }\n' > /root/.config/gtk-3.0/gtk.css && \
    # 同时为 /home/user 添加配置（因为 HOME=/home/user）
    mkdir -p /home/user/.config/gtk-3.0 && \
    cp /root/.config/gtk-3.0/gtk.css /home/user/.config/gtk-3.0/gtk.css && \
    # 禁用 XFCE 会话的 root 警告（系统级配置）
    mkdir -p /root/.config/xfce4/xfconf/xfce-perchannel-xml && \
    echo '<?xml version="1.0" encoding="UTF-8"?>\n<channel name="xfce4-session" version="1.0">\n  <property name="general" type="empty">\n    <property name="ShowRootWarning" type="bool" value="false"/>\n  </property>\n</channel>' > /root/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-session.xml && \
    # 同时为 user 配置
    mkdir -p /home/user/.config/xfce4/xfconf/xfce-perchannel-xml && \
    cp /root/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-session.xml /home/user/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-session.xml && \
    # ========== 系统级 im-config ==========
    echo 'fcitx5' > /etc/X11/xinit/xinputrc 2>/dev/null || true && \
    # ========== 用户目录 - 只保留需要持久化的配置 ==========
    # Chromium 配置 (用户可能需要持久化浏览器数据)
    mkdir -p /home/user/.config/chromium/Default/ && \
    echo '{  "browser": { "custom_chrome_frame": true, "has_seen_welcome_page": true }, "profile": { "default_content_setting_values": { "geolocation": 1 }, "exit_type": "Normal", "exited_cleanly": true, "password_manager_enabled": false }, "credentials_enable_service": false, "credentials_enable_autosignin": false }' > /home/user/.config/chromium/Default/Preferences && \
    # GNOME keyring (禁用密码提示)
    mkdir -p /home/user/.local/share/keyrings && \
    printf '[keyring]\ndisplay-name=Default keyring\nctime=1732780800\nmtime=1732780800\nlock-on-idle=false\nlock-after=false\n' > /home/user/.local/share/keyrings/Default_keyring.keyring && \
    echo 'Default_keyring' > /home/user/.local/share/keyrings/default && \
    chmod 600 /home/user/.local/share/keyrings/Default_keyring.keyring && \
    chmod 600 /home/user/.local/share/keyrings/default && \
    # 创建基本的用户 .bashrc (只包含别名，不包含输入法配置)
    printf '# User aliases\nalias chromium="/usr/local/bin/chromium"\nalias chrome="/usr/local/bin/chromium"\nalias code="code --no-sandbox"\n' > /home/user/.bashrc && \
    # 设置权限（安全配置：755 允许读取和执行，所有者可写）
    chown -R user:user /home/user /run/user/$(id -u user) && \
    chmod 755 /home/user && \
    chmod 700 /run/user/$(id -u user) && \
    chown -R 1000:1000 /home/user/.config && \
    chown -R 1000:1000 /home/user/.local && \
    # Clean up package cache
    apt-get autoremove -y && \
    apt-get autoclean && \
    rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*

# Copy Chromium browser launcher script with fcitx support (runs as root, HOME=/home/user)
# IMPORTANT: Replace /usr/bin/chromium with our launcher so XFCE's exo-open uses it
COPY chromium-browser-launcher.sh /usr/bin/chromium-browser-launcher
RUN chmod 755 /usr/bin/chromium-browser-launcher && \
    # Backup original chromium wrapper
    mv /usr/bin/chromium /usr/bin/chromium-bin && \
    # Replace /usr/bin/chromium with our launcher (XFCE uses this path)
    cp /usr/bin/chromium-browser-launcher /usr/bin/chromium && \
    ln -sf /usr/bin/chromium-browser-launcher /usr/local/bin/chromium

# Copy MCP-specific Chromium wrapper script (runs as user to connect to fcitx5)
COPY chromium-for-mcp.sh /usr/local/bin/chromium-for-mcp
RUN chmod 755 /usr/local/bin/chromium-for-mcp

# Create a socat service with supervisor to ensure port forwarding is always running
# RUN mkdir -p /etc/supervisor/conf.d && \
#     echo '[supervisord]\nnodaemon=true\n\n[program:socat]\ncommand=socat TCP4-LISTEN:9223,fork,reuseaddr TCP4:127.0.0.1:9222\nautostart=true\nautorestart=true\nstdout_logfile=/var/log/socat.log\nstdout_logfile_maxbytes=1MB\nstdout_logfile_backups=10\nstderr_logfile=/var/log/socat.err\nstderr_logfile_maxbytes=1MB\nstderr_logfile_backups=10' > /etc/supervisor/conf.d/socat.conf

# Start the socat service at system boot
# RUN echo '#!/bin/bash\n# Start supervisor to manage socat service\n/usr/bin/supervisord -c /etc/supervisor/supervisord.conf &' > /usr/local/bin/start-port-forward.sh && \
#     chmod +x /usr/local/bin/start-port-forward.sh && \
#     echo "[Desktop Entry]\nType=Application\nName=PortForward\nExec=/usr/local/bin/start-port-forward.sh\nHidden=false\nNoDisplay=false\nX-GNOME-Autostart-enabled=true" > /home/user/.config/autostart/port-forward.desktop

# 添加自动启动supervisord的桌面启动项 (使用系统级配置)
RUN echo '#!/bin/bash\n# 启动supervisord服务\nsudo /usr/bin/supervisord -c /etc/supervisor/supervisord.conf &' > /usr/local/bin/start-supervisord.sh && \
    chmod +x /usr/local/bin/start-supervisord.sh && \
    echo "[Desktop Entry]\nType=Application\nName=Supervisord\nExec=/usr/local/bin/start-supervisord.sh\nHidden=false\nNoDisplay=false\nX-GNOME-Autostart-enabled=true" > /etc/xdg/autostart/supervisord.desktop && \
    chmod 644 /etc/xdg/autostart/supervisord.desktop

# Install VS Code - 使用 apt 仓库方式（比直接下载 deb 更可靠）
RUN echo "📦 Adding Microsoft VS Code repository..." && \
    # 添加微软 GPG 密钥
    curl -fsSL https://packages.microsoft.com/keys/microsoft.asc | gpg --dearmor -o /usr/share/keyrings/microsoft-archive-keyring.gpg && \
    # 添加 apt 仓库
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/microsoft-archive-keyring.gpg] https://packages.microsoft.com/repos/code stable main" > /etc/apt/sources.list.d/vscode.list && \
    echo "📥 Installing VS Code via apt..." && \
    apt-get update && \
    apt-get install -y code && \
    echo "🔍 Checking installed VS Code..." && \
    which code && \
    ls -la /usr/bin/code && \
    echo "✅ VS Code installed via apt!" && \
    # Cleanup
    apt-get autoremove -y && \
    apt-get autoclean && \
    rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*

# 验证关键软件安装是否成功
RUN echo "🔍 Verifying installations..." && \
    # VS Code - 检查多个可能的路径
    CODE_PATH="" && \
    if [ -x /usr/bin/code ]; then CODE_PATH="/usr/bin/code"; \
    elif [ -x /usr/share/code/bin/code ]; then CODE_PATH="/usr/share/code/bin/code"; \
    elif command -v code >/dev/null 2>&1; then CODE_PATH="$(command -v code)"; \
    fi && \
    if [ -z "$CODE_PATH" ]; then \
    echo "❌ VS Code installation failed! No 'code' binary found" && \
    echo "📂 Searching for code binary..." && \
    find /usr -name "code" -type f 2>/dev/null | head -10 && \
    exit 1; \
    fi && \
    echo "✅ VS Code found at: $CODE_PATH" && \
    # 如果 /usr/bin/code 不存在，创建软链接
    if [ ! -x /usr/bin/code ] && [ -n "$CODE_PATH" ]; then \
    ln -sf "$CODE_PATH" /usr/bin/code && \
    echo "🔗 Created symlink: /usr/bin/code -> $CODE_PATH"; \
    fi && \
    # Chromium
    if [ ! -x /usr/bin/chromium ]; then \
    echo "❌ Chromium installation failed!" && exit 1; \
    fi && \
    echo "✅ Chromium: $(/usr/bin/chromium --version 2>/dev/null || echo 'installed')" && \
    echo "🎉 All critical software verified!"

RUN mkdir -p /home/user/.config/Code/User
COPY ./settings.json /home/user/.config/Code/User/settings.json

# Copy desktop background for XFCE
COPY ./wallpaper.png /usr/share/backgrounds/xfce/wallpaper.png
# Copy xfce4-desktop.xml to system-level config
RUN mkdir -p /etc/xdg/xfce4/xfconf/xfce-perchannel-xml/
COPY ./xfce4-desktop.xml /etc/xdg/xfce4/xfconf/xfce-perchannel-xml/xfce4-desktop.xml

# ========== 系统级 XFCE 配置 (不受用户目录挂载影响) ==========
RUN mkdir -p /etc/xdg/xfce4/xfconf/xfce-perchannel-xml/ && \
    # Disable screen saver
    echo '<?xml version="1.0" encoding="UTF-8"?>\n<channel name="xfce4-screensaver" version="1.0">\n  <property name="saver" type="empty">\n    <property name="enabled" type="bool" value="false"/>\n    <property name="mode" type="int" value="0"/>\n  </property>\n  <property name="lock" type="empty">\n    <property name="enabled" type="bool" value="false"/>\n  </property>\n</channel>' > /etc/xdg/xfce4/xfconf/xfce-perchannel-xml/xfce4-screensaver.xml && \
    # Disable power management screen dimming
    echo '<?xml version="1.0" encoding="UTF-8"?>\n<channel name="xfce4-power-manager" version="1.0">\n  <property name="xfce4-power-manager" type="empty">\n    <property name="blank-on-ac" type="int" value="0"/>\n    <property name="blank-on-battery" type="int" value="0"/>\n    <property name="dpms-enabled" type="bool" value="false"/>\n    <property name="dpms-on-ac-sleep" type="uint" value="0"/>\n    <property name="dpms-on-ac-off" type="uint" value="0"/>\n    <property name="dpms-on-battery-sleep" type="uint" value="0"/>\n    <property name="dpms-on-battery-off" type="uint" value="0"/>\n  </property>\n</channel>' > /etc/xdg/xfce4/xfconf/xfce-perchannel-xml/xfce4-power-manager.xml && \
    # Configure XFCE panel with top bar (panel-1) and bottom dock (panel-2)
    echo '<?xml version="1.0" encoding="UTF-8"?>\n<channel name="xfce4-panel" version="1.0">\n  <property name="configver" type="int" value="2"/>\n  <property name="panels" type="array">\n    <value type="int" value="1"/>\n    <value type="int" value="2"/>\n    <property name="dark-mode" type="bool" value="true"/>\n    <property name="panel-1" type="empty">\n      <property name="position" type="string" value="p=6;x=0;y=0"/>\n      <property name="length" type="uint" value="100"/>\n      <property name="position-locked" type="bool" value="true"/>\n      <property name="icon-size" type="uint" value="16"/>\n      <property name="size" type="uint" value="26"/>\n      <property name="plugin-ids" type="array">\n        <value type="int" value="1"/>\n        <value type="int" value="2"/>\n        <value type="int" value="3"/>\n        <value type="int" value="4"/>\n        <value type="int" value="5"/>\n        <value type="int" value="6"/>\n        <value type="int" value="7"/>\n        <value type="int" value="8"/>\n      </property>\n    </property>\n    <property name="panel-2" type="empty">\n      <property name="autohide-behavior" type="uint" value="1"/>\n      <property name="position" type="string" value="p=10;x=0;y=0"/>\n      <property name="length" type="uint" value="1"/>\n      <property name="position-locked" type="bool" value="true"/>\n      <property name="size" type="uint" value="48"/>\n      <property name="plugin-ids" type="array">\n        <value type="int" value="15"/>\n        <value type="int" value="16"/>\n        <value type="int" value="17"/>\n        <value type="int" value="18"/>\n        <value type="int" value="19"/>\n        <value type="int" value="20"/>\n        <value type="int" value="21"/>\n      </property>\n    </property>\n  </property>\n  <property name="plugins" type="empty">\n    <property name="plugin-1" type="string" value="applicationsmenu"/>\n    <property name="plugin-2" type="string" value="tasklist">\n      <property name="grouping" type="uint" value="1"/>\n    </property>\n    <property name="plugin-3" type="string" value="separator">\n      <property name="expand" type="bool" value="true"/>\n      <property name="style" type="uint" value="0"/>\n    </property>\n    <property name="plugin-4" type="string" value="systray">\n      <property name="square-icons" type="bool" value="true"/>\n    </property>\n    <property name="plugin-5" type="string" value="separator">\n      <property name="style" type="uint" value="0"/>\n    </property>\n    <property name="plugin-6" type="string" value="notification-plugin"/>\n    <property name="plugin-7" type="string" value="clock"/>\n    <property name="plugin-8" type="string" value="actions"/>\n    <property name="plugin-15" type="string" value="showdesktop"/>\n    <property name="plugin-16" type="string" value="separator"/>\n    <property name="plugin-17" type="string" value="launcher">\n      <property name="items" type="array">\n        <value type="string" value="xfce4-terminal-emulator.desktop"/>\n      </property>\n    </property>\n    <property name="plugin-18" type="string" value="launcher">\n      <property name="items" type="array">\n        <value type="string" value="xfce4-file-manager.desktop"/>\n      </property>\n    </property>\n    <property name="plugin-19" type="string" value="launcher">\n      <property name="items" type="array">\n        <value type="string" value="chromium.desktop"/>\n      </property>\n    </property>\n    <property name="plugin-20" type="string" value="launcher">\n      <property name="items" type="array">\n        <value type="string" value="xfce4-appfinder.desktop"/>\n      </property>\n    </property>\n    <property name="plugin-21" type="string" value="directorymenu"/>\n  </property>\n</channel>' > /etc/xdg/xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml && \
    # Create launcher directories for bottom dock (系统级)
    mkdir -p /etc/xdg/xfce4/panel/launcher-17 && \
    mkdir -p /etc/xdg/xfce4/panel/launcher-18 && \
    mkdir -p /etc/xdg/xfce4/panel/launcher-19 && \
    mkdir -p /etc/xdg/xfce4/panel/launcher-20 && \
    # Create terminal launcher (plugin-17)
    printf '[Desktop Entry]\nVersion=1.0\nType=Application\nExec=xfce4-terminal\nIcon=org.xfce.terminalemulator\nStartupNotify=true\nTerminal=false\nCategories=Utility;X-XFCE;\nName=Terminal Emulator\nComment=Use the command line\n' > /etc/xdg/xfce4/panel/launcher-17/xfce4-terminal-emulator.desktop && \
    # Create file manager launcher (plugin-18)
    printf '[Desktop Entry]\nVersion=1.0\nType=Application\nExec=thunar\nIcon=org.xfce.filemanager\nStartupNotify=true\nTerminal=false\nCategories=Utility;X-XFCE;\nName=File Manager\nComment=Browse the file system\n' > /etc/xdg/xfce4/panel/launcher-18/xfce4-file-manager.desktop && \
    # Create Chromium launcher (plugin-19) - replaces default Web Browser
    printf '[Desktop Entry]\nVersion=1.0\nType=Application\nName=Chromium\nComment=Web Browser\nExec=/usr/bin/chromium-browser-launcher\nIcon=chromium\nTerminal=false\nCategories=Network;WebBrowser;\nStartupNotify=true\n' > /etc/xdg/xfce4/panel/launcher-19/chromium.desktop && \
    # Create app finder launcher (plugin-20)
    printf '[Desktop Entry]\nVersion=1.0\nType=Application\nExec=xfce4-appfinder\nIcon=org.xfce.appfinder\nStartupNotify=true\nTerminal=false\nCategories=Utility;X-XFCE;\nName=Application Finder\nComment=Find and launch applications\n' > /etc/xdg/xfce4/panel/launcher-20/xfce4-appfinder.desktop && \
    # Create system-wide Chromium desktop entry
    mkdir -p /usr/share/applications && \
    printf "[Desktop Entry]\nVersion=1.0\nType=Application\nName=Chromium\nComment=Web Browser\nExec=/usr/bin/chromium-browser-launcher\nIcon=chromium\nTerminal=false\nCategories=Network;WebBrowser;" > /usr/share/applications/chromium-browser.desktop && \
    ln -sf /usr/share/applications/chromium-browser.desktop /usr/share/applications/chromium.desktop && \
    chmod 644 /usr/share/applications/chromium-browser.desktop /usr/share/applications/chromium.desktop && \
    # Set Chromium as default web browser (系统级)
    mkdir -p /usr/share/applications && \
    echo "[Default Applications]\ntext/html=chromium.desktop\ntext/xml=chromium.desktop\napplication/xhtml+xml=chromium.desktop\napplication/xml=chromium.desktop\napplication/rss+xml=chromium.desktop\napplication/rdf+xml=chromium.desktop\nimage/gif=chromium.desktop\nimage/jpeg=chromium.desktop\nimage/png=chromium.desktop\nx-scheme-handler/http=chromium.desktop\nx-scheme-handler/https=chromium.desktop\nx-scheme-handler/ftp=chromium.desktop\nx-scheme-handler/chrome=chromium.desktop\nvideo/webm=chromium.desktop\napplication/x-xpinstall=chromium.desktop" > /usr/share/applications/mimeapps.list

# Create desktop icons for terminal, VS Code, and Chrome
RUN mkdir -p /home/user/Desktop /usr/share/applications && \
    # Create safe desktop files in system directory (not in user's home)
    printf "[Desktop Entry]\nVersion=1.0\nType=Application\nName=Terminal\nComment=Terminal Emulator\nExec=xfce4-terminal\nIcon=utilities-terminal\nTerminal=false\nCategories=System;TerminalEmulator;" > /usr/share/applications/user-terminal.desktop && \
    printf "[Desktop Entry]\nVersion=1.0\nType=Application\nName=Visual Studio Code\nComment=Code Editor\nExec=/usr/bin/code --no-sandbox --user-data-dir=/home/user/.config/Code\nIcon=vscode\nTerminal=false\nCategories=Development;IDE;" > /usr/share/applications/user-vscode.desktop && \
    chmod 644 /usr/share/applications/user-terminal.desktop /usr/share/applications/user-vscode.desktop && \
    # Create symbolic links from Desktop to system apps (安全软链接)
    ln -sf /usr/share/applications/user-terminal.desktop /home/user/Desktop/terminal.desktop && \
    ln -sf /usr/share/applications/user-vscode.desktop /home/user/Desktop/vscode.desktop && \
    ln -sf /usr/share/applications/chromium-browser.desktop /home/user/Desktop/chromium.desktop && \
    chown -R user:user /home/user/Desktop

# ========== 创建骨架目录 /etc/skel-user-desktop ==========
# 将 /home/user 下的关键配置备份到此目录
# 启动时，如果检测到 /home/user 是空的（被挂载覆盖），则从这里恢复
# 这样可以解决：
#   1. 挂载空目录到 /home/user 导致桌面图标消失
#   2. 挂载空目录到 /home/user 导致 VNC 花屏（缺少关键缓存目录）
RUN mkdir -p /etc/skel-user-desktop && \
    # 复制整个 /home/user 目录结构到骨架目录
    cp -a /home/user/. /etc/skel-user-desktop/ && \
    # 确保骨架目录只读（防止意外修改）
    chmod 755 /etc/skel-user-desktop && \
    echo "✅ Skeleton directory created at /etc/skel-user-desktop"

# Copy firefox policies
COPY firefox-policies.json /usr/lib/firefox-esr/distribution/policies.json
COPY firefox-autoconfig.js /usr/lib/firefox-esr/defaults/pref/autoconfig.js
COPY firefox.cfg /usr/lib/firefox-esr/firefox.cfg


# Code interpretor environment setup
# 系统依赖和Python 3安装 (merged for smaller image size)
RUN DEBIAN_FRONTEND=noninteractive apt-get update && \
    apt-get install -y --no-install-recommends \
    build-essential curl git util-linux jq sudo fonts-noto-cjk \
    systemd ca-certificates chrony r-base software-properties-common \
    libzmq3-dev libzmq5 pkg-config && \
    # 使用 Debian 12 默认的 Python 3.11 (更稳定)
    apt-get install -y python3 python3-dev python3-venv python3-pip && \
    ln -sf /usr/bin/python3 /usr/bin/python && \
    # Python/Jupyter services and kernels removed, but Python3 environment kept
    # Clean up package cache to reduce image size:
    apt-get autoremove -y && \
    apt-get autoclean && \
    rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*

# 安装 Node.js 22.x (现在支持，因为不再依赖 ijavascript)
RUN apt-get update && \
    apt-get install -y curl gnupg ca-certificates && \
    # 添加 NodeSource PPA for Node.js 22.x
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && \
    # 安装 Node.js (npm 会一同被安装)
    apt-get install -y nodejs && \
    # 更新证书
    update-ca-certificates && \
    # 配置 pnpm
    corepack enable && \
    corepack prepare pnpm@latest --activate && \
    echo "Node.js version: $(node -v)" && \
    echo "npm version: $(npm -v)" && \
    echo "pnpm version: $(pnpm -v)" && \
    # Clean up package cache to reduce image size:
    apt-get autoremove -y && \
    apt-get autoclean && \
    rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*

# 环境变量设置
ENV R_VERSION=4.4.2

ENV R_HOME=/opt/R/${R_VERSION} \
    JAVA_HOME=/opt/java/openjdk

# # 创建默认用户
# RUN useradd -ms /bin/bash user && \
#     usermod -aG sudo user && \
#     passwd -d user && \
#     echo "user ALL=(ALL:ALL) NOPASSWD: ALL" >>/etc/sudoers

# 设置目录权限（安全配置）
RUN mkdir -p /home/user && \
    chown -R user:user /home/user && \
    chmod 755 /home/user && \
    chmod 755 /usr/local

# Python/Jupyter components removed

# JavaScript/TypeScript 使用 Deno kernel (不需要安装 ijavascript)
# Deno kernel 在上面已经安装并配置完成

# Deno runtime (for JavaScript/TypeScript support)
COPY --from=denoland/deno:bin-2.5.6 /deno /usr/bin/deno
RUN chmod +x /usr/bin/deno && \
    # Clean up deno cache
    rm -rf /root/.cache/deno
# Note: deno.json not copied as Jupyter is removed

# Bash Kernel (Python-based) removed

# R Kernel
# RUN R -e "install.packages('IRkernel', repos='https://cloud.r-project.org'); IRkernel::installspec(user = FALSE, name = 'r', displayname = 'R')"


# envd 服务已删除 - 不再需要环境守护进程

# 配置 chrony
RUN mkdir -p /etc/chrony
RUN echo 'makestep 1 -1' > /etc/chrony/chrony.conf

RUN mkdir -p /etc/systemd/system/chrony.service.d
RUN echo '[Service]\n\
    ExecStart=\n\
    ExecStart=/usr/sbin/chronyd\n\
    User=root\n\
    Group=root' > /etc/systemd/system/chrony.service.d/override.conf

RUN systemctl enable chrony

# Java 配置 (without Jupyter integration)
COPY --from=eclipse-temurin:17-jdk $JAVA_HOME $JAVA_HOME
RUN rm -f /usr/bin/java && \
    ln -s ${JAVA_HOME}/bin/java /usr/bin/java

# 配置自动登录
RUN mkdir -p /etc/systemd/system/serial-getty@ttyS0.service.d
RUN echo '[Service]\n\
    ExecStart=\n\
    ExecStart=-/sbin/agetty --noissue --autologin root %I 115200,38400,9600 vt102' \
    > /etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf

# Python server components removed

# 复制启动脚本
COPY ./start-up.sh /root/
RUN chmod +x /root/start-up.sh

# 复制中文输入法修复脚本（容器内运行 fix-ime 命令）
COPY ./fix-chrome-ime-final.sh /usr/local/bin/fix-ime
RUN chmod +x /usr/local/bin/fix-ime


# IPython/Jupyter config files removed

ENV DISPLAY=:0

# 设置全局输入法环境变量（容器级别 - 纯 fcitx）
ENV GTK_IM_MODULE=fcitx \
    QT_IM_MODULE=fcitx \
    XMODIFIERS=@im=fcitx \
    INPUT_METHOD=fcitx \
    SDL_IM_MODULE=fcitx \
    GLFW_IM_MODULE=ibus

# 配置 D-Bus 和 PolicyKit（已在前面安装 dbus-x11）
RUN mkdir -p /etc/dbus-1/session.d && \
    printf '<!-- Session bus for user -->\n<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">\n<busconfig>\n  <policy user="user">\n    <allow own="*"/>\n    <allow own_prefix="*"/>\n    <allow send_destination="*"/>\n    <allow receive_sender="*"/>\n  </policy>\n</busconfig>' > /etc/dbus-1/session.d/user-session.conf && \
    printf '<!-- Allow root to access user session bus for fcitx5 input method -->\n<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">\n<busconfig>\n  <policy user="root">\n    <allow own="*"/>\n    <allow own_prefix="*"/>\n    <allow send_destination="*"/>\n    <allow receive_sender="*"/>\n    <allow eavesdrop="true"/>\n  </policy>\n</busconfig>' > /etc/dbus-1/session.d/allow-root.conf && \
    # Set proper permissions
    chown -R root:root /etc/polkit-1 /etc/dbus-1 && \
    chmod 755 /etc/polkit-1/localauthority/50-local.d /etc/dbus-1/session.d && \
    chmod 644 /etc/polkit-1/localauthority/50-local.d/*.pkla /etc/dbus-1/session.d/*.conf



# 安装 agent_runner 运行时依赖（与 rcoder/Dockerfile-runner 保持一致）
RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    wget \
    gnupg \
    unzip \
    zip \
    tzdata \
    lsof \
    iproute2 \
    net-tools \
    nginx \
    && rm -rf /var/lib/apt/lists/*

# 设置系统时区为东八区（Asia/Shanghai）
RUN ln -sf /usr/share/zoneinfo/Asia/Shanghai /etc/localtime && \
    echo "Asia/Shanghai" > /etc/timezone

# 清理默认nginx配置以避免冲突
RUN rm -f /etc/nginx/sites-enabled/default && \
    rm -f /etc/nginx/conf.d/default.conf


# 配置 npm 使用国内镜像源
RUN npm config set registry https://registry.npmmirror.com

# 设置工作目录并确保权限
RUN mkdir -p /app/computer-project-workspace /home/user/.cache && \
    chown -R user:user /app /home/user

# 安装 Bun 到全局路径（需要 root）
ENV BUN_CONFIG_REGISTRY="https://registry.npmmirror.com" \
    BUN_CONFIG_INSTALL_CACHE_DIR="/home/user/.cache/bun/install" \
    BUN_CONFIG_GLOBAL_CACHE_DIR="/home/user/.cache/bun/global" \
    BUN_INSTALL="/usr/local" \
    PATH="/usr/local/bin:$PATH"

RUN curl -fsSL https://bun.sh/install | bash && \
    /usr/local/bin/bun --version

# 安装全局 npm 包（需要 root，因为安装到 /usr/lib/node_modules）
RUN npm install -g @zed-industries/claude-code-acp@latest && \
    npm install -g @anthropic-ai/claude-code@latest && \
    npm install -g chrome-devtools-mcp@latest

# 安装 OpenSSL 开发包和构建依赖（mcp-stdio-proxy 需要）
RUN apt-get update && apt-get install -y \
    libssl-dev \
    pkg-config \
    build-essential && \
    rm -rf /var/lib/apt/lists/*

# 安装 Rust toolchain 并安装 mcp-stdio-proxy（需要 root）
ENV RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PATH="/opt/cargo/bin:$PATH"
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && \
    chmod -R 755 /opt/rustup /opt/cargo && \
    . /opt/cargo/env && \
    cargo install mcp-stdio-proxy && \
    # 确保 user 可以使用 cargo
    chmod -R o+rX /opt/cargo /opt/rustup

# 安装 uv 到全局路径（需要 root）
ENV UV_INSTALL_DIR=/usr/local/bin \
    UV_CACHE_DIR=/home/user/.cache/uv
RUN curl -LsSf https://astral.sh/uv/install.sh | sh && \
    echo 'export UV_INDEX_URL=https://mirrors.aliyun.com/pypi/simple' >> /etc/profile.d/uv-env.sh && \
    echo 'export UV_CACHE_DIR=/home/user/.cache/uv' >> /etc/profile.d/uv-env.sh

# 添加 cargo bin 和 uv 到 PATH
ENV PATH="/opt/cargo/bin:/usr/local/bin:${PATH}"

# 复制预构建的 agent_runner 二进制文件（需要 root）
COPY bin/agent_runner /usr/local/bin/agent_runner
RUN chmod +x /usr/local/bin/agent_runner

# ============================================================================
# 🔊 pcmflux 音频流服务安装
# ============================================================================
# 安装 pcmflux Python 包（需要 root，用于编译 C++ 扩展）
RUN pip3 install websockets && \
    pip3 install git+https://github.com/linuxserver/pcmflux.git && \
    echo "✅ pcmflux audio streaming installed"

# 复制音频服务脚本和静态文件
COPY audio_server.py /usr/local/bin/audio_server.py
COPY audio_static /usr/local/share/audio_static
RUN chmod +x /usr/local/bin/audio_server.py

# 暴露音频流 HTTP 端口
EXPOSE 6090

# ============================================================================
# 🎯 切换到普通用户 - 后续操作以 user 身份执行
# ============================================================================
USER user
WORKDIR /home/user

# 配置 pnpm（用户级配置）
RUN pnpm config set registry https://registry.npmmirror.com && \
    pnpm config set store-dir /home/user/.cache/pnpm

# 配置 Bun 用户配置文件
RUN mkdir -p /home/user/.cache/bun/install /home/user/.cache/bun/global && \
    echo '[install]' > /home/user/.bunfig.toml && \
    echo 'registry = "https://registry.npmmirror.com"' >> /home/user/.bunfig.toml && \
    echo '' >> /home/user/.bunfig.toml && \
    echo '[install.cache]' >> /home/user/.bunfig.toml && \
    echo 'dir = "/home/user/.cache/bun"' >> /home/user/.bunfig.toml && \
    cat /home/user/.bunfig.toml && \
    echo "✅ Bun configured for user"

# 预安装 context7 MCP 工具（以 user 身份，缓存在用户目录）
RUN bun add -g @upstash/context7-mcp || echo "⚠️ bun add -g failed, will retry at runtime"

# 创建 Claude Code 配置目录
RUN mkdir -p /home/user/.claude && \
    echo '{"permissions":{"deny":["WebFetch", "WebSearch"]}}' > /home/user/.claude/settings.json

# 预安装 MCP 工具到 uv 缓存中（以 user 身份）
RUN mkdir -p /home/user/.cache/uv && \
    UV_CACHE_DIR=/home/user/.cache/uv uv tool install mcp-server-fetch || echo "⚠️ uv tool install failed, will retry at runtime"

# ============================================================================
# 🎯 切回 root - 复制启动脚本 & 更新骨架目录
# ============================================================================
USER root

# 更新骨架目录（包含 user 阶段创建的配置）
# 这样当宿主机挂载空目录到 /home/user 时，start-up.sh 可以从这里恢复
RUN echo "📦 Updating skeleton directory with user configs..." && \
    cp -a /home/user/. /etc/skel-user-desktop/ && \
    echo "✅ Skeleton directory updated at /etc/skel-user-desktop"

# 暴露 agent_runner 端口
EXPOSE 8086

# 设置 agent_runner 默认端口环境变量
ENV AGENT_RUNNER_PORT=8086

ENTRYPOINT ["/root/start-up.sh"]

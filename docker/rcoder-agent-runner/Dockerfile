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
    # Clone NoVNC and websockify:
    git clone -b e2b-desktop https://github.com/e2b-dev/noVNC.git /opt/noVNC && \
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
    wget -O /tmp/libreoffice.tar.gz "https://mirrors.ustc.edu.cn/tdf/libreoffice/stable/25.8.1/deb/${LO_ARCH}/LibreOffice_25.8.1_Linux_${LO_ARCH2}_deb.tar.gz" && \
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
    echo '{"AllowInsecureLocalConnections": true, "RemoteDebuggingAllowed": true, "RemoteDebuggingPort": "9222", "RemoteDebuggingAddress":"0.0.0.0", "AutoplayAllowed": true, "MetricsReportingEnabled": false, "BrowserSignin": 0, "SyncDisabled": true, "PromotionalTabsEnabled": false, "DefaultBrowserSettingEnabled": false, "DefaultSearchProviderEnabled": false, "HideWebStoreIcon": true, "PasswordManagerEnabled": false, "PromptForDownloadLocation": false, "SafeBrowsingEnabled": false, "SafeBrowsingExtendedReportingEnabled": false, "UrlKeyedAnonymizedDataCollectionEnabled": false, "CloudPrintSubmitEnabled": false, "CloudManagementEnrollmentToken": "", "SpellcheckEnabled": false}' > /etc/chromium/policies/managed/policies.json

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
    # 设置权限
    chown -R user:user /home/user /run/user/$(id -u user) && \
    chmod 777 /home/user && \
    chmod 700 /run/user/$(id -u user) && \
    # Create Chromium wrapper script with fcitx5 support
    printf '#!/bin/bash\n# Source fcitx5 environment\nif [ -f /etc/profile.d/fcitx5-env.sh ]; then\n    source /etc/profile.d/fcitx5-env.sh\nfi\n# Load D-Bus address\nif [ -f /tmp/dbus-session-env ]; then\n    source /tmp/dbus-session-env\nfi\n# Load Chromium environment\nif [ -f /etc/profile.d/chromium-env.sh ]; then\n    source /etc/profile.d/chromium-env.sh\nfi\nexport DISPLAY=:0\nexport LANG=C.UTF-8\nexport LC_ALL=C.UTF-8\nexport LC_CTYPE=C.UTF-8\n# Use standard Chromium config directory\nCHROMIUM_DATA_DIR="${CHROMIUM_USER_DATA_DIR:-/home/user/.config/chromium}"\n# Launch Chromium\nexec /usr/bin/chromium \\\n  --user-data-dir="$CHROMIUM_DATA_DIR" \\\n  --no-sandbox \\\n  --disable-dev-shm-usage \\\n  --remote-debugging-port=9222 \\\n  --remote-debugging-address=0.0.0.0 \\\n  --no-first-run \\\n  --no-default-browser-check \\\n  --password-store=basic \\\n  --use-mock-keychain \\\n  --disable-session-crashed-bubble \\\n  --disable-infobars \\\n  --no-process-singleton-dialog \\\n  --force-color-profile=srgb "$@"\n' > /usr/bin/chromium-browser-launcher && \
    chmod 755 /usr/bin/chromium-browser-launcher && \
    chown -R 1000:1000 /home/user/.config && \
    chown -R 1000:1000 /home/user/.local && \
    # Clean up package cache
    apt-get autoremove -y && \
    apt-get autoclean && \
    rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*

# Copy MCP-specific Chromium wrapper script (runs as user to connect to fcitx5)
COPY chromium-for-mcp.sh /usr/local/bin/chromium-for-mcp
RUN chmod 755 /usr/local/bin/chromium-for-mcp

# Create Chromium launcher with fcitx5 support
RUN printf '#!/bin/bash\n# 加载 D-Bus 地址\nif [ -f /tmp/dbus-session-env ]; then\n    source /tmp/dbus-session-env\nfi\n\n# 加载 Chromium 环境变量（持久化数据目录）\nif [ -f /etc/profile.d/chromium-env.sh ]; then\n    source /etc/profile.d/chromium-env.sh\nfi\n\n# 设置输入法环境变量\nexport DISPLAY=:0\nexport GTK_IM_MODULE=fcitx5\nexport QT_IM_MODULE=fcitx5\nexport XMODIFIERS=@im=fcitx5\nexport INPUT_METHOD=fcitx5\n\n# 使用标准 Chromium 配置目录（由 start-up.sh 设置）\nCHROMIUM_DATA_DIR="${CHROMIUM_USER_DATA_DIR:-/home/user/.config/chromium}"\n\n# 以 user 用户身份运行 Chromium（禁用密码管理）\nsu - user -c "export DISPLAY=:0; export DBUS_SESSION_BUS_ADDRESS=\\\"$DBUS_SESSION_BUS_ADDRESS\\\"; export GTK_IM_MODULE=fcitx5; export QT_IM_MODULE=fcitx5; export XMODIFIERS=@im=fcitx5; export INPUT_METHOD=fcitx5; export CHROMIUM_USER_DATA_DIR=\\\"${CHROMIUM_DATA_DIR}\\\"; /usr/bin/chromium --user-data-dir=\\\"${CHROMIUM_DATA_DIR}\\\" --no-sandbox --disable-dev-shm-usage --remote-debugging-port=9222 --remote-debugging-address=0.0.0.0 --no-first-run --no-default-browser-check --password-store=basic --use-mock-keychain"\n' > /usr/local/bin/chromium && \
    chmod +x /usr/local/bin/chromium

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

# Install VS Code (optimized single layer)
ARG TARGETARCH
RUN case "$TARGETARCH" in \
    "amd64") VS_ARCH="x64" ;; \
    "arm64") VS_ARCH="arm64" ;; \
    *) echo "Unsupported architecture: $TARGETARCH" && exit 1 ;; \
    esac && \
    wget -O /tmp/vscode.deb "https://code.visualstudio.com/sha/download?build=stable&os=linux-deb-${VS_ARCH}" && \
    dpkg -i /tmp/vscode.deb || true && \
    apt-get update && \
    apt-get install -f -y && \
    # Clean up package cache to reduce image size:
    apt-get autoremove -y && \
    apt-get autoclean && \
    rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*
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
    printf "[Desktop Entry]\nVersion=1.0\nType=Application\nName=Visual Studio Code\nComment=Code Editor\nExec=/usr/bin/code --no-sandbox\nIcon=vscode\nTerminal=false\nCategories=Development;IDE;" > /usr/share/applications/user-vscode.desktop && \
    chmod 644 /usr/share/applications/user-terminal.desktop /usr/share/applications/user-vscode.desktop && \
    # Create symbolic links from Desktop to system apps (安全软链接)
    ln -sf /usr/share/applications/user-terminal.desktop /home/user/Desktop/terminal.desktop && \
    ln -sf /usr/share/applications/user-vscode.desktop /home/user/Desktop/vscode.desktop && \
    ln -sf /usr/share/applications/chromium-browser.desktop /home/user/Desktop/chromium.desktop && \
    chown -R user:user /home/user/Desktop

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

# 设置目录权限
RUN mkdir -p /home/user && \
    chmod 777 -R /home/user && \
    chmod 777 -R /usr/local

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


# 配置 envd 服务
ARG TARGETARCH
RUN mkdir -p /etc/systemd/system/multi-user.target.wants
# 复制所有 envd 二进制文件
COPY ./bin/ /tmp/envd-bin/
# 根据目标架构选择对应的 envd 二进制文件
RUN if [ "$TARGETARCH" = "arm64" ]; then \
    cp /tmp/envd-bin/envd-arm64 /usr/bin/envd; \
    else \
    cp /tmp/envd-bin/envd /usr/bin/envd; \
    fi && \
    chmod +x /usr/bin/envd && \
    rm -rf /tmp/envd-bin
RUN echo '[Unit]\n\
    Description=Env Daemon Service\n\
    \n\
    [Service]\n\
    Type=simple\n\
    Restart=always\n\
    User=root\n\
    Group=root\n\
    ExecStart=/bin/bash -l -c /usr/bin/envd\n\
    \n\
    [Install]\n\
    WantedBy=multi-user.target' > /etc/systemd/system/envd.service

RUN ln -s /etc/systemd/system/envd.service /etc/systemd/system/multi-user.target.wants/envd.service

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

# 设置全局输入法环境变量（容器级别 - 纯 fcitx5）
ENV GTK_IM_MODULE=fcitx5 \
    QT_IM_MODULE=fcitx5 \
    XMODIFIERS=@im=fcitx5 \
    INPUT_METHOD=fcitx5 \
    SDL_IM_MODULE=fcitx5 \
    GLFW_IM_MODULE=fcitx5

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

# 清理默认nginx配置以避免冲突
RUN rm -f /etc/nginx/sites-enabled/default && \
    rm -f /etc/nginx/conf.d/default.conf


# 配置 npm 使用国内镜像源
RUN npm config set registry https://registry.npmmirror.com

# 创建 pnpm 全局存储目录
RUN mkdir -p /app/computer-project-workspace/.pnpm-store
# 配置 pnpm 使用新的存储路径
RUN pnpm config set store-dir /app/computer-project-workspace/.pnpm-store



# 配置 Bun 使用国内镜像源
ENV BUN_CONFIG_REGISTRY="https://registry.npmmirror.com" \
    BUN_CONFIG_INSTALL_CACHE_DIR="/root/.cache/bun/install" \
    BUN_CONFIG_GLOBAL_CACHE_DIR="/root/.cache/bun/global" \
    BUN_INSTALL="/root/.bun" \
    PATH="/root/.bun/bin:$PATH"

# 安装 Bun
RUN curl -fsSL https://bun.sh/install | bash && \
    # 创建 Bun 配置目录和缓存目录
    mkdir -p /root/.bun /root/.cache/bun/install /root/.cache/bun/global && \
    # 设置权限
    chmod -R 777 /root/.cache/bun && \
    # 验证安装
    /root/.bun/bin/bun --version

# 配置 Bun 国内镜像源和缓存目录（根据官方文档）
RUN echo '[install]' > /root/.bunfig.toml && \
    echo 'registry = "https://registry.npmmirror.com"' >> /root/.bunfig.toml && \
    echo '' >> /root/.bunfig.toml && \
    echo '[install.cache]' >> /root/.bunfig.toml && \
    echo 'dir = "/root/.cache/bun"' >> /root/.bunfig.toml && \
    # 创建缓存目录
    mkdir -p /root/.cache/bun && \
    # 验证配置
    cat /root/.bunfig.toml && \
    echo "Bun configured with Chinese mirror and cache dir: /root/.cache/bun"

# 创建 Claude Code 配置目录并禁用 webfetch 功能，添加 mcp-fetch MCP 工具
RUN mkdir -p /root/.claude && \
    echo '{"permissions":{"deny":["WebFetch", "WebSearch"]},"mcpServers":{"mcp-fetch":{"args":["mcp-server-fetch"],"command":"uvx"}}}' > /root/.claude/settings.json

# 安装额外的命令工具（强制更新到最新版本）
RUN npm install -g @zed-industries/claude-code-acp@latest

# 安装 Claude CLI 工具（强制更新到最新版本）
RUN npm install -g @anthropic-ai/claude-code@latest

# 预安装 chrome-devtools-mcp（加速 MCP 工具启动）
RUN npm install -g chrome-devtools-mcp@latest

# 安装 uv
RUN curl -LsSf https://astral.sh/uv/install.sh | sh

# 添加uv到PATH
ENV PATH="/root/.local/bin:${PATH}"

# 设置 uv 镜像加速地址
ENV UV_INDEX_URL=https://mirrors.aliyun.com/pypi/simple


# 预安装 MCP 工具到 uv 缓存中
RUN uv tool install mcp-server-fetch




# 预安装 context7 MCP 工具到 bun 全局缓存
RUN bun add -g --registry=https://registry.npmmirror.com @upstash/context7-mcp
# 创建 Claude Code 配置目录并禁用 webfetch 功能，添加 MCP 工具
RUN mkdir -p /root/.claude && \
    echo '{"permissions":{"deny":["WebFetch", "WebSearch"]},"mcpServers":{"mcp-fetch":{"args":["mcp-server-fetch"],"command":"uvx"},"context7":{"args":["-y","@upstash/context7-mcp"],"command":"bunx","env":{}}}}' > /root/.claude/settings.json


# 安装 OpenSSL 开发包和构建依赖（mcp-stdio-proxy 需要）
RUN apt-get update && apt-get install -y \
    libssl-dev \
    pkg-config \
    build-essential && \
    rm -rf /var/lib/apt/lists/*

# 第一步: 安装 Rust toolchain 并安装 mcp-stdio-proxy
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && \
    . /root/.cargo/env && \
    cargo install mcp-stdio-proxy

# 添加 cargo bin 和 uv 到 PATH（用于后续容器运行时）
ENV PATH="/root/.cargo/bin:/root/.local/bin:${PATH}"

# 复制预构建的 agent_runner 二进制文件（由 Dockerfile.build 在 debian:12 中编译）
COPY bin/agent_runner /usr/local/bin/agent_runner
RUN chmod +x /usr/local/bin/agent_runner

# 暴露 agent_runner 端口
EXPOSE 8086

# 设置 agent_runner 默认端口环境变量
ENV AGENT_RUNNER_PORT=8086

ENTRYPOINT ["/root/start-up.sh"]

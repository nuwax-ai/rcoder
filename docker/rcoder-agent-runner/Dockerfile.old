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
    mkdir -p /home/user/.config/autostart && \
    # Create PolicyKit authentication agent autostart
    printf "[Desktop Entry]\nType=Application\nName=PolicyKit Auth\nExec=/usr/lib/policykit-1-gnome/polkit-gnome-authentication-agent-1\nHidden=false\nNoDisplay=false\nX-GNOME-Autostart-enabled=true\nStartupNotify=false" > /home/user/.config/autostart/polkit-gnome-auth.desktop && \
    # Create D-Bus session autostart
    printf "[Desktop Entry]\nType=Application\nName=D-Bus Session\nExec=/usr/bin/dbus-launch --sh-syntax\nHidden=false\nNoDisplay=false\nX-GNOME-Autostart-enabled=true" > /home/user/.config/autostart/dbus-session.desktop && \
    # Create PolicyKit daemon autostart
    printf "[Desktop Entry]\nType=Application\nName=PolicyKit Daemon\nExec=/usr/lib/policykit-1/polkitd --no-debug\nHidden=false\nNoDisplay=false\nX-GNOME-Autostart-enabled=true" > /home/user/.config/autostart/polkitd.desktop && \
    # Create Chromium data directory and set permissions
    mkdir -p /home/user/chromium-data && \
    # 创建用户运行时目录
    mkdir -p /run/user/$(id -u user) && \
    # ========== 配置中文输入法 fcitx5 ==========
    mkdir -p /home/user/.config/fcitx5 && \
    mkdir -p /home/user/.config/autostart && \
    mkdir -p /home/user/.config/environment.d && \
    # 1. 在 .bashrc 中设置环境变量（终端使用 - 纯 fcitx5）
    echo 'export GTK_IM_MODULE=fcitx5' >> /home/user/.bashrc && \
    echo 'export QT_IM_MODULE=fcitx5' >> /home/user/.bashrc && \
    echo 'export XMODIFIERS=@im=fcitx5' >> /home/user/.bashrc && \
    echo 'export INPUT_METHOD=fcitx5' >> /home/user/.bashrc && \
    echo 'export SDL_IM_MODULE=fcitx5' >> /home/user/.bashrc && \
    echo 'export GLFW_IM_MODULE=fcitx5' >> /home/user/.bashrc && \
    # 2. 在 .xprofile 中设置环境变量（X 会话启动时加载，GUI 应用可用）
    echo 'export GTK_IM_MODULE=fcitx5' > /home/user/.xprofile && \
    echo 'export QT_IM_MODULE=fcitx5' >> /home/user/.xprofile && \
    echo 'export XMODIFIERS=@im=fcitx5' >> /home/user/.xprofile && \
    echo 'export INPUT_METHOD=fcitx5' >> /home/user/.xprofile && \
    echo 'export SDL_IM_MODULE=fcitx5' >> /home/user/.xprofile && \
    echo 'export GLFW_IM_MODULE=fcitx5' >> /home/user/.xprofile && \
    # 3. 在 systemd 用户环境中设置（现代 Linux 推荐方式）
    echo 'GTK_IM_MODULE=fcitx5' > /home/user/.config/environment.d/fcitx5.conf && \
    echo 'QT_IM_MODULE=fcitx5' >> /home/user/.config/environment.d/fcitx5.conf && \
    echo 'XMODIFIERS=@im=fcitx5' >> /home/user/.config/environment.d/fcitx5.conf && \
    echo 'INPUT_METHOD=fcitx5' >> /home/user/.config/environment.d/fcitx5.conf && \
    echo 'SDL_IM_MODULE=fcitx5' >> /home/user/.config/environment.d/fcitx5.conf && \
    echo 'GLFW_IM_MODULE=fcitx5' >> /home/user/.config/environment.d/fcitx5.conf && \
    # 4. 创建自启动配置 - 让 fcitx5 在 XFCE 启动后自动启动
    printf '[Desktop Entry]\nType=Application\nName=Fcitx 5\nComment=Input Method Framework\nExec=fcitx5 -d --replace\nIcon=fcitx5\nStartupNotify=false\nTerminal=false\nCategories=System;Utility;\nX-GNOME-Autostart-enabled=true\nX-XFCE-Autostart-enabled=true' > /home/user/.config/autostart/fcitx5.desktop && \
    # 6. 配置 fcitx5 全局选项（启用云拼音、设置切换快捷键）
    mkdir -p /home/user/.config/fcitx5/conf && \
    printf '[Hotkey]\n# Enumerate when press trigger key repeatedly\nEnumerateWithTriggerKeys=True\n# Temporally switch between first and current Input Method\nAltTriggerKeys=\n# Enumerate Input Method Forward\nEnumerateForwardKeys=\n# Enumerate Input Method Backward\nEnumerateBackwardKeys=\n# Skip first input method while enumerating\nEnumerateSkipFirst=False\n\n[Hotkey/TriggerKeys]\n0=Control+space\n1=Shift+Control+space\n\n[Hotkey/EnumerateGroupForwardKeys]\n0=Super+space\n\n[Hotkey/EnumerateGroupBackwardKeys]\n0=Shift+Super+space\n\n[Hotkey/ActivateKeys]\n0=Hangul_Hanja\n\n[Hotkey/DeactivateKeys]\n0=Hangul_Romaja\n\n[Hotkey/PrevPage]\n0=Up\n\n[Hotkey/NextPage]\n0=Down\n\n[Hotkey/PrevCandidate]\n0=Shift+Tab\n\n[Hotkey/NextCandidate]\n0=Tab\n\n[Hotkey/TogglePreedit]\n0=Control+Alt+P\n\n[Behavior]\n# Active By Default\nActiveByDefault=True\n# Share Input State\nShareInputState=No\n# Preload input method to be used by default\nPreloadInputMethod=True\n# Show Input Method Information when switch input method\nShowInputMethodInformation=True\n# Show Input Method Information when changing focus\nShowInputMethodInformationWhenFocusIn=False\n# Show compact input method information\nCompactInputMethodInformation=True\n# Show first input method information\nShowFirstInputMethodInformation=True\n# Default page size\nDefaultPageSize=5\n# Override Xkb Option\nOverrideXkbOption=False\n# Custom Xkb Option\nCustomXkbOption=\n# Force Enabled Addons\nEnabledAddons=\n# Force Disabled Addons\nDisabledAddons=\n# Preload input method to be used by default\nPreeditEnabledByDefault=True' > /home/user/.config/fcitx5/config && \
    # 7. 配置拼音输入法选项（禁用云拼音提示）
    mkdir -p /home/user/.config/fcitx5/conf && \
    printf '[Addon]\n# Enabled\nEnabled=True\n\n[Behavior]\n# Show preedit within application\nPreeditInApplication=False\n\n[PinyinEngine]\n# Shuangpin Profile\nShuangpinProfile=Ziranma\n# Show current shuangpin mode\nShowActualPinyinInHint=False\n# Enable Prediction\nPredictionEnabled=True\n# Prediction Size\nPredictionSize=10\n# Enable Cloud Pinyin (默认禁用，避免首次提示)\nCloudPinyinEnabled=False\n# Cloud Pinyin Index\nCloudPinyinIndex=2' > /home/user/.config/fcitx5/conf/pinyin.conf && \
    # 7.1 配置云拼音选项（禁用首次提示）
    printf '[Addon]\n# Enabled\nEnabled=False\n\n[Behavior]\n# Show notification when toggled\nShowNotification=False' > /home/user/.config/fcitx5/conf/cloudpinyin.conf && \
    # 7.2 禁用 fcitx5 所有通知
    printf '[Addon]\nEnabled=False\n\n[Behavior]\nHiddenNotifications=' > /home/user/.config/fcitx5/conf/notifications.conf && \
    # 8. 创建输入法启用列表（关键：确保拼音输入法被启用）
    mkdir -p /home/user/.local/share/fcitx5/inputmethod && \
    printf 'keyboard-us:True\npinyin:True' > /home/user/.local/share/fcitx5/inputmethod/default.conf && \
    # 9. 启用拼音输入法 addon
    mkdir -p /home/user/.config/fcitx5/conf && \
    printf '[Addons]\n0=fcitx5-pinyin\n1=fcitx5-chinese-addons' > /home/user/.config/fcitx5/addon.conf && \
    # 10. 创建简化的 profile（使用更兼容的格式）
    printf '[Groups/0]\nName=Default\nDefault Layout=us\nDefaultIM=pinyin\n\n[Groups/0/Items/0]\nName=keyboard-us\nLayout=\n\n[Groups/0/Items/1]\nName=pinyin\nLayout=\n\n[GroupOrder]\n0=Default\n\n[Behavior]\nActiveByDefault=True\nShareInputState=No' > /home/user/.config/fcitx5/profile && \
    # 11. 配置 im-config（系统级输入法框架选择）
    mkdir -p /home/user/.config && \
    echo 'fcitx5' > /home/user/.config/im-config && \
    # 12. 创建 GTK 3.0 im-module 配置（使用 fcitx5）
    mkdir -p /home/user/.config/gtk-3.0 && \
    printf '[Settings]\ngtk-im-module=fcitx5' > /home/user/.config/gtk-3.0/settings.ini && \
    chown -R user:user /home/user /run/user/$(id -u user) && \
    chmod -R 777 /home/user/chromium-data && \
    chmod 777 /home/user && \
    chmod 700 /run/user/$(id -u user) && \
    # Create Chromium wrapper script with fcitx5 support
    printf '#!/bin/bash\n# 加载 D-Bus 地址\nif [ -f /tmp/dbus-session-env ]; then\n    source /tmp/dbus-session-env\nfi\n\n# 设置输入法环境变量\nexport DISPLAY=:0\nexport GTK_IM_MODULE=fcitx5\nexport QT_IM_MODULE=fcitx5\nexport XMODIFIERS=@im=fcitx5\nexport INPUT_METHOD=fcitx5\n\n# 启动 Chromium（禁用密码管理）\nexec /usr/bin/chromium \\\n  --user-data-dir=/home/user/chromium-data \\\n  --no-sandbox \\\n  --disable-dev-shm-usage \\\n  --remote-debugging-port=9222 \\\n  --remote-debugging-address=0.0.0.0 \\\n  --no-first-run \\\n  --no-default-browser-check \\\n  --password-store=basic \\\n  --use-mock-keychain "$@"\n' > /usr/bin/chromium-browser-launcher && \
    chmod 755 /usr/bin/chromium-browser-launcher && \
    # Add Chromium aliases
    echo 'alias chromium="/usr/local/bin/chromium"' >> /home/user/.bashrc && \
    echo 'alias chrome="/usr/local/bin/chromium"' >> /home/user/.bashrc && \
    # Add VS Code alias
    echo 'alias code="code --no-sandbox"' >> /home/user/.bashrc && \
    chown -R 1000:1000 /home/user/.config && \
    chown -R 1000:1000 /home/user/.local && \
    # Clean up package cache to reduce image size:
    apt-get autoremove -y && \
    apt-get autoclean && \
    rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*

# Create Chromium launcher with fcitx5 support
RUN printf '#!/bin/bash\n# 加载 D-Bus 地址\nif [ -f /tmp/dbus-session-env ]; then\n    source /tmp/dbus-session-env\nfi\n\n# 设置输入法环境变量\nexport DISPLAY=:0\nexport GTK_IM_MODULE=fcitx5\nexport QT_IM_MODULE=fcitx5\nexport XMODIFIERS=@im=fcitx5\nexport INPUT_METHOD=fcitx5\n\n# 以 user 用户身份运行 Chromium（禁用密码管理）\nsu - user -c "export DISPLAY=:0; export DBUS_SESSION_BUS_ADDRESS=\\\"$DBUS_SESSION_BUS_ADDRESS\\\"; export GTK_IM_MODULE=fcitx5; export QT_IM_MODULE=fcitx5; export XMODIFIERS=@im=fcitx5; export INPUT_METHOD=fcitx5; /usr/bin/chromium --user-data-dir=/home/user/chromium-data --no-sandbox --disable-dev-shm-usage --remote-debugging-port=9222 --remote-debugging-address=0.0.0.0 --no-first-run --no-default-browser-check --password-store=basic --use-mock-keychain"\n' > /usr/local/bin/chromium && \
    chmod +x /usr/local/bin/chromium

# Create a socat service with supervisor to ensure port forwarding is always running
# RUN mkdir -p /etc/supervisor/conf.d && \
#     echo '[supervisord]\nnodaemon=true\n\n[program:socat]\ncommand=socat TCP4-LISTEN:9223,fork,reuseaddr TCP4:127.0.0.1:9222\nautostart=true\nautorestart=true\nstdout_logfile=/var/log/socat.log\nstdout_logfile_maxbytes=1MB\nstdout_logfile_backups=10\nstderr_logfile=/var/log/socat.err\nstderr_logfile_maxbytes=1MB\nstderr_logfile_backups=10' > /etc/supervisor/conf.d/socat.conf

# Start the socat service at system boot
# RUN echo '#!/bin/bash\n# Start supervisor to manage socat service\n/usr/bin/supervisord -c /etc/supervisor/supervisord.conf &' > /usr/local/bin/start-port-forward.sh && \
#     chmod +x /usr/local/bin/start-port-forward.sh && \
#     echo "[Desktop Entry]\nType=Application\nName=PortForward\nExec=/usr/local/bin/start-port-forward.sh\nHidden=false\nNoDisplay=false\nX-GNOME-Autostart-enabled=true" > /home/user/.config/autostart/port-forward.desktop

# 添加自动启动supervisord的桌面启动项
RUN echo '#!/bin/bash\n# 启动supervisord服务\nsudo /usr/bin/supervisord -c /etc/supervisor/supervisord.conf &' > /usr/local/bin/start-supervisord.sh && \
    chmod +x /usr/local/bin/start-supervisord.sh && \
    echo "[Desktop Entry]\nType=Application\nName=Supervisord\nExec=/usr/local/bin/start-supervisord.sh\nHidden=false\nNoDisplay=false\nX-GNOME-Autostart-enabled=true" > /home/user/.config/autostart/supervisord.desktop && \
    chmod +x /home/user/.config/autostart/supervisord.desktop && \
    chown -R user:user /home/user/.config/autostart

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
RUN mkdir -p /home/user/.config/xfce4/xfconf/xfce-perchannel-xml/
COPY ./xfce4-desktop.xml /home/user/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-desktop.xml

# Disable screen saver and power management, configure XFCE panel
RUN mkdir -p /home/user/.config/xfce4/xfconf/xfce-perchannel-xml/ && \
    echo '<?xml version="1.0" encoding="UTF-8"?>\n<channel name="xfce4-screensaver" version="1.0">\n  <property name="saver" type="empty">\n    <property name="enabled" type="bool" value="false"/>\n    <property name="mode" type="int" value="0"/>\n  </property>\n  <property name="lock" type="empty">\n    <property name="enabled" type="bool" value="false"/>\n  </property>\n</channel>' > /home/user/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-screensaver.xml && \
    echo '<?xml version="1.0" encoding="UTF-8"?>\n<channel name="xfce4-power-manager" version="1.0">\n  <property name="xfce4-power-manager" type="empty">\n    <property name="blank-on-ac" type="int" value="0"/>\n    <property name="blank-on-battery" type="int" value="0"/>\n    <property name="dpms-enabled" type="bool" value="false"/>\n    <property name="dpms-on-ac-sleep" type="uint" value="0"/>\n    <property name="dpms-on-ac-off" type="uint" value="0"/>\n    <property name="dpms-on-battery-sleep" type="uint" value="0"/>\n    <property name="dpms-on-battery-off" type="uint" value="0"/>\n  </property>\n</channel>' > /home/user/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-power-manager.xml && \
    # Configure XFCE panel with Chromium launcher in taskbar
    echo '<?xml version="1.0" encoding="UTF-8"?>\n<channel name="xfce4-panel" version="1.0">\n  <property name="configver" type="int" value="2"/>\n  <property name="panels" type="array">\n    <value type="int" value="1"/>\n    <property name="panel-1" type="empty">\n      <property name="position" type="string" value="p=6;x=0;y=0"/>\n      <property name="length" type="uint" value="100"/>\n      <property name="position-locked" type="bool" value="true"/>\n      <property name="size" type="uint" value="30"/>\n      <property name="plugin-ids" type="array">\n        <value type="int" value="1"/>\n        <value type="int" value="2"/>\n        <value type="int" value="3"/>\n        <value type="int" value="4"/>\n        <value type="int" value="5"/>\n        <value type="int" value="6"/>\n        <value type="int" value="7"/>\n      </property>\n    </property>\n  </property>\n  <property name="plugins" type="empty">\n    <property name="plugin-1" type="string" value="applicationsmenu"/>\n    <property name="plugin-2" type="string" value="separator"/>\n    <property name="plugin-3" type="string" value="tasklist"/>\n    <property name="plugin-4" type="string" value="launcher">\n      <property name="items" type="array">\n        <value type="string" value="chromium.desktop"/>\n      </property>\n    </property>\n    <property name="plugin-5" type="string" value="separator"/>\n    <property name="plugin-6" type="string" value="systray"/>\n    <property name="plugin-7" type="string" value="clock"/>\n  </property>\n</channel>' > /home/user/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml && \
    # Create system-wide Chromium desktop entry (支持 fcitx5 输入法)
    mkdir -p /usr/share/applications && \
    printf "[Desktop Entry]\nVersion=1.0\nType=Application\nName=Chromium\nComment=Web Browser\nExec=/usr/bin/chromium-browser-launcher\nIcon=chromium\nTerminal=false\nCategories=Network;WebBrowser;" > /usr/share/applications/chromium-browser.desktop && \
    ln -sf /usr/share/applications/chromium-browser.desktop /usr/share/applications/chromium.desktop && \
    chmod 644 /usr/share/applications/chromium-browser.desktop /usr/share/applications/chromium.desktop && \
    # Override user-level as well
    mkdir -p /home/user/.local/share/applications && \
    printf "[Desktop Entry]\nVersion=1.0\nType=Application\nName=Chromium\nComment=Web Browser\nExec=/usr/bin/chromium-browser-launcher\nIcon=chromium\nTerminal=false\nCategories=Network;WebBrowser;" > /home/user/.local/share/applications/chromium.desktop && \
    chmod 644 /home/user/.local/share/applications/chromium.desktop && \
    # Set Chromium as default web browser
    echo "[Default Applications]\ntext/html=chromium.desktop\ntext/xml=chromium.desktop\napplication/xhtml+xml=chromium.desktop\napplication/xml=chromium.desktop\napplication/rss+xml=chromium.desktop\napplication/rdf+xml=chromium.desktop\nimage/gif=chromium.desktop\nimage/jpeg=chromium.desktop\nimage/png=chromium.desktop\nx-scheme-handler/http=chromium.desktop\nx-scheme-handler/https=chromium.desktop\nx-scheme-handler/ftp=chromium.desktop\nx-scheme-handler/chrome=chromium.desktop\nvideo/webm=chromium.desktop\napplication/x-xpinstall=chromium.desktop" > /home/user/.config/mimeapps.list && \
    chown -R user:user /home/user/.config /home/user/.local

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
# 系统依赖和Python 3.12安装 (merged for smaller image size)
RUN DEBIAN_FRONTEND=noninteractive apt-get update && \
    apt-get install -y --no-install-recommends \
    build-essential curl git util-linux jq sudo fonts-noto-cjk \
    systemd ca-certificates chrony r-base software-properties-common \
    libzmq3-dev libzmq5 pkg-config && \
    # 使用 Debian 12 默认的 Python 3.11 (更稳定)
    apt-get install -y python3 python3-dev python3-venv python3-pip && \
    ln -sf /usr/bin/python3 /usr/bin/python && \
    # curl -sS https://bootstrap.pypa.io/get-pip.py | python && \
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
ENV PIP_DEFAULT_TIMEOUT=100 \
    PIP_DISABLE_PIP_VERSION_CHECK=1 \
    PIP_NO_CACHE_DIR=1 \
    JUPYTER_CONFIG_PATH="/root/.jupyter" \
    IPYTHON_CONFIG_PATH="/root/.ipython" \
    SERVER_PATH="/root/.server" \
    R_VERSION=4.4.2

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

# 安装 Jupyter (with cleanup)
COPY ./requirements.txt requirements.txt
RUN pip install --break-system-packages --upgrade pip && \
    pip install --break-system-packages --no-cache-dir -r requirements.txt && \
    ipython kernel install --name "python3" --user && \
    # Clean up pip cache
    rm -rf /root/.cache

# JavaScript/TypeScript 使用 Deno kernel (不需要安装 ijavascript)
# Deno kernel 在上面已经安装并配置完成

# Deno Kernel (upgraded to latest 2.5.6)
COPY --from=denoland/deno:bin-2.5.6 /deno /usr/bin/deno
RUN chmod +x /usr/bin/deno && \
    deno jupyter --unstable --install && \
    # Clean up deno cache
    rm -rf /root/.cache/deno
COPY ./deno.json /root/.local/share/jupyter/kernels/deno/kernel.json

# Bash Kernel (with cleanup)
RUN pip install --break-system-packages bash_kernel && \
    python -m bash_kernel.install && \
    # Clean up pip cache
    rm -rf /root/.cache

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

# Java 配置 (with cleanup)
COPY --from=eclipse-temurin:11-jdk $JAVA_HOME $JAVA_HOME
RUN rm -f /usr/bin/java && \
    ln -s ${JAVA_HOME}/bin/java /usr/bin/java && \
    wget https://github.com/SpencerPark/IJava/releases/download/v1.3.0/ijava-1.3.0.zip && \
    unzip ijava-1.3.0.zip && \
    python install.py --sys-prefix && \
    # Clean up installation files
    rm -rf ijava-1.3.0.zip install.py java/

# 配置自动登录
RUN mkdir -p /etc/systemd/system/serial-getty@ttyS0.service.d
RUN echo '[Service]\n\
    ExecStart=\n\
    ExecStart=-/sbin/agetty --noissue --autologin root %I 115200,38400,9600 vt102' \
    > /etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf

# 创建虚拟环境并安装服务器依赖 (with cleanup)
RUN python -m venv $SERVER_PATH/.venv
COPY ./server/requirements.txt $SERVER_PATH
RUN $SERVER_PATH/.venv/bin/pip install --no-cache-dir -r $SERVER_PATH/requirements.txt && \
    # Clean up pip cache in virtual environment
    rm -rf $SERVER_PATH/.venv/pip-cache $SERVER_PATH/.venv/lib/python*/site-packages/pip/_vendor/distlib/*.whl
COPY ./server $SERVER_PATH

# 复制配置文件
COPY matplotlibrc /root/.config/matplotlib/.matplotlibrc
COPY ./start-up.sh $JUPYTER_CONFIG_PATH/
RUN chmod +x $JUPYTER_CONFIG_PATH/start-up.sh
COPY ./jupyter_server_config.py $JUPYTER_CONFIG_PATH/

# 复制中文输入法修复脚本（容器内运行 fix-ime 命令）
COPY ./fix-chrome-ime-final.sh /usr/local/bin/fix-ime
RUN chmod +x /usr/local/bin/fix-ime


RUN mkdir -p $IPYTHON_CONFIG_PATH/profile_default
COPY ipython_kernel_config.py $IPYTHON_CONFIG_PATH/profile_default/
RUN mkdir -p $IPYTHON_CONFIG_PATH/profile_default/startup
COPY startup_scripts/* $IPYTHON_CONFIG_PATH/profile_default/startup

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
    # Set proper permissions
    chown -R root:root /etc/polkit-1 /etc/dbus-1 && \
    chmod 755 /etc/polkit-1/localauthority/50-local.d /etc/dbus-1/session.d && \
    chmod 644 /etc/polkit-1/localauthority/50-local.d/*.pkla /etc/dbus-1/session.d/*.conf




ENTRYPOINT $JUPYTER_CONFIG_PATH/start-up.sh

#/bin/bash -l -c "/usr/bin/envd"

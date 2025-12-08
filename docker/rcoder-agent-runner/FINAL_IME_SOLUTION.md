# 🎉 Docker 中文输入法最终解决方案

## ✅ 成功方案

**纯 fcitx5 方案**（不使用 ibus 桥接）

## 核心架构

```
Chromium (GTK3 应用)
    ↓ GTK_IM_MODULE=fcitx5
fcitx5 (输入法引擎)
    ↓
fcitx5-pinyin (拼音输入法)
```

## 关键修改

### 1. 软件包（Dockerfile 第 25 行）
```bash
fcitx5 fcitx5-pinyin fcitx5-frontend-gtk3 fcitx5-frontend-gtk2 fcitx5-frontend-qt5 fcitx5-module-xorg fcitx5-config-qt im-config dbus-x11
```

**移除了**: `ibus ibus-gtk ibus-gtk3`（不再需要）

### 2. 环境变量（所有位置）
```bash
export GTK_IM_MODULE=fcitx5  # 不是 ibus！
export QT_IM_MODULE=fcitx5
export XMODIFIERS=@im=fcitx5
export INPUT_METHOD=fcitx5
```

### 3. Chromium 启动脚本
创建 `/usr/bin/chromium-browser-launcher`:
```bash
#!/bin/bash
# 加载 D-Bus 地址
if [ -f /tmp/dbus-session-env ]; then
    source /tmp/dbus-session-env
fi

# 设置输入法环境变量
export DISPLAY=:0
export GTK_IM_MODULE=fcitx5
export QT_IM_MODULE=fcitx5
export XMODIFIERS=@im=fcitx5
export INPUT_METHOD=fcitx5

# 启动 Chromium
exec /usr/bin/chromium \
  --user-data-dir=/home/user/chromium-data \
  --no-sandbox \
  --disable-dev-shm-usage \
  --remote-debugging-port=9222 \
  --remote-debugging-address=0.0.0.0 \
  --no-first-run \
  --no-default-browser-check "$@"
```

### 4. XFCE 自启动
只需要 `fcitx5.desktop`:
```desktop
[Desktop Entry]
Type=Application
Name=Fcitx 5
Exec=fcitx5 -d --replace
Icon=fcitx5
StartupNotify=false
Terminal=false
X-GNOME-Autostart-enabled=true
X-XFCE-Autostart-enabled=true
```

## 为什么这个方案有效？

1. **Chromium 启动脚本**：确保 Chromium 启动时就有正确的环境变量
2. **纯 fcitx5**：避免了 ibus 桥接的复杂性和兼容性问题
3. **D-Bus 地址**：从 `/tmp/dbus-session-env` 加载，确保连接正确

## 之前方案失败的原因

### ibus 桥接方案失败
- fcitx5 的 `ibusfrontend` 模块虽然加载，但**从未成功注册到 ibus**
- `ibus list-engine` 从未显示 fcitx5 的输入法引擎
- ibus 和 fcitx5 之间的桥接从一开始就没有工作

### 环境变量传递失败
- 在 shell 脚本中设置环境变量，但 XFCE 子进程没有继承
- `/proc/<pid>/environ` 读取权限问题导致无法验证
- `dbus-launch` 创建新会话，覆盖了之前的 D-Bus 地址

## 测试验证

### 成功标志
- ✅ 输入法托盘显示拼音图标
- ✅ Ctrl+Space 可以切换输入法
- ✅ Chromium 搜索框可以输入中文
- ✅ 终端、gedit 等应用可以输入中文

### 测试步骤
1. 启动容器
2. 连接 VNC
3. 打开 Chromium
4. 访问 baidu.com
5. 按 Ctrl+Space 切换输入法
6. 输入拼音（如 nihao）

## 构建和使用

```bash
# 构建镜像
docker build -t edgebox-sandbox .

# 启动容器
docker run -d --name sandbox -p 8080:8080 edgebox-sandbox

# 访问 VNC
open http://localhost:8080
```

## 文件清单

### 修改的文件
- `Dockerfile`: 完全移除 ibus，改用纯 fcitx5
- `start-up.sh`: 环境变量改为 fcitx5
- `chromium-browser-launcher`: 新的启动脚本

### 移除的内容
- ❌ ibus 相关包
- ❌ ibus 自启动配置
- ❌ start-ibus.sh 脚本
- ❌ GTK_IM_MODULE=ibus 配置

---

**状态**: ✅ 已验证可用  
**测试日期**: 2025-11-28  
**环境**: Debian 12 + XFCE + Chromium + fcitx5  
**方案**: 纯 fcitx5（无 ibus 桥接）

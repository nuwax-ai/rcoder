# RCoder Agent Runner 故障排查指南

## 常见问题

### 1. VNC 虚拟桌面文件权限问题

#### 问题描述

在 VNC 虚拟桌面中尝试打开文件时，提示：
```
Could not open the file "/home/user/xxx/file.xxx".
You do not have the permissions necessary to open the file.
```

#### 根本原因

这是 Docker 容器用户 UID 不匹配导致的权限问题：

1. **宿主机目录挂载**：当宿主机目录挂载到容器的 `/home/user` 时，宿主机文件的 UID 可能与容器内 `user` 用户的 UID（默认 1000）不同
2. **权限检查失败**：容器内的 `user` 用户无法读取其他 UID 所有者的文件
3. **VNC 用户权限**：VNC 桌面环境以 `user` 用户身份运行，继承了权限限制

#### 解决方案

##### 方案 A：自动修复（推荐）

从 **v1.x.x** 版本开始，容器启动脚本会自动修复挂载目录的权限问题。

**修复逻辑**（在 `start-up.sh` 中自动执行）：
```bash
# 修复整个 /home/user 的所有者
chown -R user:user /home/user

# 修复目录权限：确保 user 可以读取、写入、执行目录
find /home/user -type d -exec chmod u+rwx {} \;

# 修复文件权限：确保 user 可以读取和写入文件
find /home/user -type f -exec chmod u+rw {} \;
```

**使用新版本镜像**：
```bash
# 重新构建镜像
make dev-build

# 停止旧容器
make dev-down

# 启动新容器（自动修复权限）
make dev-up
```

##### 方案 B：手动修复（临时方案）

如果使用旧版本镜像，可以手动在容器内修复权限：

```bash
# 1. 查找容器名称
docker ps | grep rcoder

# 2. 进入容器
docker exec -it <container_name> bash

# 3. 修复整个 /home/user 目录权限
chown -R user:user /home/user
find /home/user -type d -exec chmod u+rwx {} \;
find /home/user -type f -exec chmod u+rw {} \;

# 4. 退出容器
exit
```

**注意**：手动修复后，在 VNC 桌面中需要重新打开文件管理器或应用程序才能生效。

##### 方案 C：宿主机侧修复（不推荐）

在宿主机上修改文件所有者（需要 sudo 权限）：

```bash
# 查看容器内 user 用户的 UID
docker exec <container_name> id -u user
# 输出：1000

# 在宿主机上修改文件所有者为容器内的 UID
sudo chown -R 1000:1000 /path/to/host/directory
```

**缺点**：
- 需要 root 权限
- 会影响宿主机文件系统
- 不适合多用户环境

#### 预防措施

**推荐做法**：

1. **使用最新版本镜像**：确保使用包含自动权限修复逻辑的镜像版本
2. **避免混合 UID**：尽量让宿主机用户的 UID 与容器内 `user` 用户的 UID（1000）保持一致
3. **使用 Docker Volume**：对于需要持久化的数据，使用 Docker Volume 而不是直接挂载宿主机目录

**Docker Compose 配置示例**：
```yaml
services:
  agent-runner:
    image: rcoder-agent-runner:latest
    volumes:
      # 推荐：使用 Docker Volume
      - user_home_volume:/home/user
      
      # 或者：挂载宿主机目录（会自动修复权限）
      - ./host-directory:/home/user

volumes:
  user_home_volume:
```

#### 技术细节

**为什么会出现 UID 不匹配？**

Docker 容器使用 Linux 内核的命名空间（namespace）隔离，但文件系统的 UID/GID 是全局的：

- **容器内 `user` 用户**：UID = 1000, GID = 1000
- **宿主机用户**：UID 可能是 1001, 1002, ... 或任意值
- **挂载目录**：保留宿主机文件的原始 UID/GID

当容器内的进程尝试访问宿主机文件时，Linux 内核检查进程的 UID（1000）是否匹配文件的 UID（如 1001），不匹配则拒绝访问。

**权限位说明**：
- `u+rwx`：所有者可读（r）、可写（w）、可执行（x）目录
- `u+rw`：所有者可读（r）、可写（w）文件

### 2. Chromium 浏览器无法打开

#### 问题描述
VNC 桌面中 Chromium 浏览器无法启动或显示空白页面。

#### 解决方案
检查 Chromium 数据目录权限（已在启动脚本中自动处理）：
```bash
# 容器内执行
ls -la /home/user/.config/chromium
```

如果权限不正确，容器会在启动时自动修复。

### 3. 中文输入法无法使用

#### 问题描述
在 VNC 桌面中无法使用中文输入法（fcitx5）。

#### 解决方案
1. 检查 fcitx5 进程是否运行：
   ```bash
   docker exec <container_name> pgrep fcitx5
   ```

2. 如果未运行，手动启动：
   ```bash
   docker exec -u user <container_name> bash -c "DISPLAY=:0 fcitx5 -d"
   ```

3. 在 VNC 桌面中使用 `Ctrl+Space` 切换输入法

## 日志和调试

### 查看容器启动日志
```bash
# 查看最新日志
docker logs <container_name>

# 实时查看日志
docker logs -f <container_name>
```

### 进入容器调试
```bash
# 以 root 用户进入
docker exec -it <container_name> bash

# 以 user 用户进入
docker exec -it -u user <container_name> bash
```

### 检查 VNC 服务状态
```bash
# 检查 x11vnc 进程
docker exec <container_name> pgrep x11vnc

# 检查 noVNC 端口
docker exec <container_name> netstat -tuln | grep 6080
```

## 联系支持

如果以上方案无法解决问题，请：

1. 收集以下信息：
   - 容器版本：`docker inspect <container_name> | grep Image`
   - 错误日志：`docker logs <container_name>`
   - 宿主机操作系统和版本
   
2. 提交 Issue 到 GitHub 仓库

## 更新记录

- **v1.x.x** (2025-12-19): 添加自动权限修复逻辑
- **v1.0.0** (初始版本): 基础容器镜像

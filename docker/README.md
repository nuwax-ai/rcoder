# Docker 本地测试配置说明

## 📊 监控服务架构

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           监控服务总览                                   │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │                    Pyroscope Server                             │   │
│  │                    CPU 性能分析                                  │   │
│  │  - 端口: 4040                                                   │   │
│  │  - 用途: 持续性能剖析                                          │   │
│  │  - 数据源: Grafana Alloy (eBPF)                                │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│                              ↓                                          │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │                    Prometheus Server                            │   │
│  │                    时序指标存储                                  │   │
│  │  - 端口: 9091 (宿主机) / 9090 (容器)                           │   │
│  │  - 用途: 存储进程指标                                          │   │
│  │  - 数据源: Grafana Alloy (Process Exporter)                    │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│                              ↓                                          │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │                    Grafana                                       │   │
│  │                    可视化平台                                    │   │
│  │  - 端口: 3000                                                   │   │
│  │  - 用途: 进程监控 Dashboard                                    │   │
│  │  - 数据源: Prometheus                                          │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 监控服务快速访问

| 服务 | 地址 | 登录信息 | 用途 |
|------|------|----------|------|
| **Pyroscope** | http://localhost:4040 | 无需登录 | CPU 性能分析火焰图 |
| **Prometheus** | http://localhost:9091 | 无需登录 | 时序指标查询 |
| **Grafana** | http://localhost:3000 | admin / admin | 进程监控 Dashboard |

### Grafana Dashboard

**Dashboard 名称**: Agent Runner 进程监控

**包含面板**:
- 概览: RSS/VSZ 内存、CPU 使用率、文件描述符
- 内存趋势: RSS 和 VSZ 的时间序列图
- I/O 监控: 读取/写入速率
- 上下文切换: 自愿/非自愿切换速率
- 线程详情: 线程数量、FD 使用率
- 缺页错误: 次要/主要缺页错误速率

---

## 📁 文件说明

### `config.yml`
本地 Docker 容器测试专用配置文件，用于在 docker-compose 启动的容器中测试动态启动子容器。

### `docker-compose.yml`
监控服务配置，包含 Pyroscope、Prometheus 和 Grafana。

**服务列表**:
- `rcoder`: 主 RCoder 服务
- `pyroscope`: CPU 性能分析服务器
- `prometheus`: 时序指标数据库
- `grafana`: 可视化平台

### 核心配置

#### 镜像配置
所有子容器使用与主容器相同的镜像：`master-rcoder:latest`

```yaml
services:
  rcoder:
    image: "master-rcoder:latest"

  agent-runner:
    image: "master-rcoder:latest"
```

#### 路径配置
- **项目工作目录**: `/app/project_workspace`（容器内路径）
- **日志目录**: `/app/logs`
- **规范目录**: `/app/specs`

---

## 🚀 使用方法

### 1. 构建镜像
```bash
make dev-build
```

### 2. 启动容器（包含监控服务）
```bash
make dev-up
```

监控服务会自动启动：
- Pyroscope: http://localhost:4040
- Prometheus: http://localhost:9091
- Grafana: http://localhost:3000

### 3. 测试动态容器启动
容器内的 RCoder 服务会读取 `/app/config.yml`，动态启动的子容器会使用相同的 `master-rcoder:latest` 镜像。

### 4. 查看日志
```bash
make dev-logs
```

### 5. 查看监控数据

#### Pyroscope (CPU 性能分析)
```bash
# 打开 Pyroscope Web UI
open http://localhost:4040

# 选择应用: agent_runner
# 查看火焰图、时序数据、Top 函数
```

#### Grafana Dashboard (进程指标)
```bash
# 打开 Grafana
open http://localhost:3000

# 登录: admin / admin
# 查找: "Agent Runner 进程监控"
# 选择变量: project_id, instance, process_name
```

#### Prometheus (原始指标)
```bash
# 打开 Prometheus
open http://localhost:9091

# 查询示例:
# process_resident_memory_bytes{project_id="user_124"}
# rate(process_cpu_seconds_total[30s]) * 100
```

---

## 🔄 与生产环境的区别

| 配置项 | 本地测试 | 生产环境 |
|--------|---------|---------|
| **主容器镜像** | `master-rcoder:latest` | `registry.yichamao.com/rcoder:latest-arm64` |
| **子容器镜像** | `master-rcoder:latest` | `registry.yichamao.com/rcoder:latest-arm64` |
| **配置文件** | `docker/config.yml` | `config.yml` (项目根目录) |
| **项目路径** | `/app/project_workspace` | `./project_workspace` |
| **监控服务** | 完整（Pyroscope + Prometheus + Grafana） | 按需部署 |

---

## 📝 注意事项

1. **镜像一致性**：本地测试时，主容器和子容器使用相同的镜像，确保环境一致
2. **配置隔离**：`docker/config.yml` 仅用于容器内测试，不影响宿主机配置
3. **路径映射**：容器内的路径会自动映射到宿主机的 `docker/project_workspace`
4. **网络模式**：使用 `bridge` 网络模式，容器间可以通过内部网络通信
5. **端口映射**：
   - Prometheus: 9091 (宿主机) → 9090 (容器)
   - 避免与宿主机已有的 Prometheus 冲突

---

## 🐛 故障排查

### 子容器无法启动
```bash
# 检查镜像是否存在
docker images | grep master-rcoder

# 如果镜像不存在，重新构建
make dev-build
```

### 配置文件未生效
```bash
# 检查配置文件是否正确挂载
docker exec -it <container_id> cat /app/config.yml

# 检查日志
make dev-logs
```

### 路径映射问题
```bash
# 检查挂载点
docker inspect <container_id> | grep Mounts -A 20
```

### 监控服务无数据
```bash
# 1. 检查监控服务是否运行
docker ps | grep -E "pyroscope|prometheus|grafana"

# 2. 检查 agent_runner 容器是否运行
docker ps | grep agent_runner

# 3. 检查 Prometheus 中是否有指标
curl -s 'http://localhost:9091/api/v1/query?query=process_resident_memory_bytes' | python3 -m json.tool | head -20

# 4. 检查 Alloy 日志
docker exec <container> tail -f /app/container-logs/diag/alloy.log
```

### Grafana Dashboard 显示 "No Data"

**原因**: 没有 agent_runner 容器在运行

**解决**:
1. 创建一个 agent_runner 容器（通过发送聊天请求）
2. 等待 15-30 秒让数据采集
3. 刷新 Grafana Dashboard

### Prometheus 端口冲突
如果宿主机已有 Prometheus 使用 9090 端口，docker-compose.yml 已将端口映射改为 9091。

```yaml
# docker-compose.yml
prometheus:
  ports:
    - "9091:9090"  # 宿主机 9091 → 容器 9090
```

---

## 🧪 测试页面

`test-page/` 目录包含 VNC、音频和输入法透传功能的集成测试页面。

### 文件说明

| 文件 | 说明 |
|------|------|
| `vnc-test.html` | 集成测试页面（VNC + 音频 + IME） |
| `opus-decoder.min.js` | Opus 音频解码库（86KB） |

### 功能测试

测试页面支持以下功能：
- **VNC 远程桌面**: 通过 WebSocket 连接到容器的 noVNC 服务
- **音频流播放**: 接收并播放容器的音频输出（Opus 编码）
- **输入法透传**: 使用本地输入法（如中文输入法）输入到远程桌面

### 使用方法

#### 1. 启动本地 HTTP 服务器

```bash
# 进入测试页面目录
cd /Users/soddy/Documents/git_workspace/rcoder/docker/test-page

# 使用 Python 3 启动 HTTP 服务器
python3 -m http.server 8000
```

#### 2. 访问测试页面

在浏览器中打开：
```
http://127.0.0.1:8000/vnc-test.html
```

#### 3. 配置连接参数

**RCoder 代理模式**（推荐）:
- RCoder 服务地址: `http://127.0.0.1:8088`
- User ID: `user_123`
- Project ID: 留空或填写实际项目 ID

#### 4. 创建测试容器

```bash
# 发送聊天请求，自动创建容器
curl -X POST http://127.0.0.1:8088/computer/chat \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "user_123",
    "prompt": "hello"
  }'
```

---

## 🔧 eBPF 诊断工具（开发调试）

### 概述

RCoder 集成了 eBPF 诊断工具，用于在开发环境中快速定位进程阻塞和性能问题。通过 Rust feature flag `ebpf-debug` 控制，默认在 `make dev-restart` 时启用。

### ⚠️ 安全警告

**eBPF 调试模式会启用容器特权模式**，容器将获得 `SYS_ADMIN` 权限。仅在受信任的调试环境使用！

| 模式 | Feature | 容器特权 | 安全性 | 调试能力 |
|------|---------|----------|--------|----------|
| `make dev-restart` | `ebpf-debug` 启用 | 特权 | ⚠️ 降低安全 | ✅ 完整诊断 |
| `make docker-build-agent-production` | 默认关闭 | 限制 | ✅ 高安全 | ❌ 无 eBPF |

### 监控能力

| 工具 | 类型 | 频率 | 输出 |
|------|------|------|------|
| **Alloy eBPF** | 持续 CPU 监控 | 97 Hz | Pyroscope Web UI |
| **Alloy Process Exporter** | 进程指标 | 15 秒 | Grafana Dashboard |
| **offcpu-monitor** | 阻塞火焰图 | 60 秒 | SVG 文件 |
| **syscall-monitor** | 系统调用追踪 | 60 秒 | 日志文件 |

详细文档请参考: `/docker/rcoder-agent-runner/ebpf-tools/README.md`

### 容器内诊断命令

进入容器后，可以使用以下快捷命令：

```bash
# 进入容器
docker exec -it <container> bash

# 获取 agent_runner 进程 PID
PID=$(pgrep agent_runner)

# 快捷诊断命令
e-offcpu $PID       # CPU 性能分析（显示耗时函数）
e-flame $PID 60     # 生成 60 秒火焰图
e-profile $PID      # 性能分析
e-all $PID          # 综合诊断（包含所有分析）
```

### 导出诊断数据

```bash
# 导出所有诊断数据到宿主机
docker cp <container>:/app/container-logs/diag ./diag-results

# 导出单个火焰图
docker cp <container>:/app/container-logs/diag/flame-<pid>.svg ./
```

---

## 📚 监控数据查询示例

### Prometheus 查询

```promql
# 内存使用趋势
process_resident_memory_bytes{project_id="user_124"}

# CPU 使用率
rate(process_cpu_seconds_total{project_id="user_124"}[30s]) * 100

# I/O 读取速率
rate(process_read_bytes_total{project_id="user_124"}[30s])

# 文件描述符使用率
process_open_fds{project_id="user_124"} / process_max_fds{project_id="user_124"}

# 上下文切换速率
rate(process_context_switches_total{project_id="user_124",context_switch_type="voluntary"}[30s])
```

### Grafana 变量

Dashboard 中可用的变量：
- `project_id`: 过滤项目 ID
- `instance`: 过滤实例
- `process_name`: 过滤进程名称
- `resolution`: 查询分辨率 (15s, 30s, 1m, 5m, 15m)

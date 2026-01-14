# Docker 本地测试配置说明

> 本文档说明如何在本地使用 Docker Compose 启动 RCoder 开发环境，包含完整的监控服务（Pyroscope、Prometheus、Grafana）和 eBPF 诊断工具。

## 📑 目录

- [快速开始](#-快速开始)
- [环境要求](#-环境要求)
- [架构概览](#-架构概览)
- [监控服务](#-监控服务)
- [配置说明](#-配置说明)
- [常用命令](#-常用命令)
- [测试页面](#-测试页面)
- [eBPF 诊断工具](#-ebpf-诊断工具)
- [故障排查](#-故障排查)
- [常见问题](#-常见问题)
- [目录结构](#-目录结构)

---

## ⚡ 快速开始

### 一键启动

```bash
# 1. 构建镜像
make dev-build

# 2. 启动所有服务（包含监控）
make dev-up

# 3. 查看日志
make dev-logs
```

### 访问服务

| 服务 | 地址 | 用途 |
|------|------|------|
| **Pyroscope** | http://localhost:4040 | CPU 性能分析火焰图 |
| **Prometheus** | http://localhost:9091 | 时序指标查询 |
| **Grafana** | http://localhost:3000 | 进程监控 Dashboard (admin/admin) |

### 创建测试容器

```bash
# 发送聊天请求，自动创建 agent_runner 容器
curl -X POST http://127.0.0.1:8088/computer/chat \
  -H "Content-Type: application/json" \
  -d '{"user_id": "user_123", "prompt": "hello"}'
```

---

## 💻 环境要求

### 必需组件

| 组件 | 版本要求 | 用途 |
|------|----------|------|
| **Docker** | 20.10+ | 容器运行时 |
| **Docker Compose** | 2.0+ | 多容器编排 |
| **Make** | 任意 | 构建自动化 |
| **Rust** | 1.75+ | 编译项目（本地开发） |

### 可选组件

| 组件 | 用途 |
|------|------|
| **Python 3** | 启动测试页面 HTTP 服务器 |
| **cURL** | API 测试 |

### 端口占用检查

启动前请确保以下端口未被占用：

```bash
# 检查端口占用
lsof -i :4040   # Pyroscope
lsof -i :9091   # Prometheus
lsof -i :3000   # Grafana
lsof -i :8088   # RCoder API
```

---

## 🏗️ 架构概览

### 整体架构

```mermaid
flowchart TB
    subgraph Docker["🐳 Docker Compose 环境"]
        direction TB

        RCoder["RCoder 主服务<br/>端口: 8088<br/>镜像: master-rcoder:latest<br/>功能: HTTP API + 容器管理"]
        
        subgraph Agent["Agent Runner 子容器（动态创建）"]
            Runner["AI 代理运行时<br/>gRPC 服务端<br/>镜像: master-rcoder:latest"]
        end
        
        subgraph Monitoring["📊 监控服务"]
            direction LR
            Pyro["Pyroscope<br/>:4040"]
            Prom["Prometheus<br/>:9091"]
            Graf["Grafana<br/>:3000"]
            Pyro --> Prom --> Graf
        end
        
        RCoder ==>|"gRPC<br/>内部网络"| Runner
    end
    
    style RCoder fill:#e1f5fe
    style Runner fill:#fff3e0
    style Pyro fill:#f3e5f5
    style Prom fill:#e8f5e9
    style Graf fill:#fce4ec
```

> **提示**: 在支持 Mermaid 的平台（GitHub、GitLab、IDE 插件）上，上图会渲染为交互式流程图。

### 数据流向

```mermaid
flowchart LR
    subgraph AgentRunner["Agent Runner 容器"]
        direction TB
        Alloy["Grafana Alloy"]
        
        subgraph Collectors["数据采集器"]
            EBPF["eBPF Profiler<br/>97 Hz"]
            PE["Process Exporter<br/>15s"]
        end
        
        Alloy --> EBPF
        Alloy --> PE
    end
    
    subgraph Storage["存储与分析"]
        Pyro["Pyroscope<br/>火焰图 + Top 函数"]
        Prom["Prometheus<br/>时序数据存储"]
    end
    
    subgraph Viz["可视化"]
        Graf["Grafana<br/>统一 Dashboard"]
    end
    
    EBPF -->|"CPU 性能数据"| Pyro
    PE -->|"进程指标"| Prom
    Pyro --> Graf
    Prom --> Graf
    
    style AgentRunner fill:#e3f2fd
    style Pyro fill:#f3e5f5
    style Prom fill:#e8f5e9
    style Graf fill:#fce4ec
```

---

## 📊 监控服务

### 服务概览

| 服务 | 端口 | 登录信息 | 数据源 | 用途 |
|------|------|----------|--------|------|
| **Pyroscope** | 4040 | 无需登录 | Alloy eBPF (97 Hz) | CPU 性能分析火焰图 |
| **Prometheus** | 9091 | 无需登录 | Alloy Process Exporter (15s) | 时序指标存储 |
| **Grafana** | 3000 | admin / admin | Prometheus | 可视化 Dashboard |

### Grafana Dashboard

**Dashboard 名称**: `Agent Runner 进程监控`

**包含面板**:
- **概览**: RSS/VSZ 内存、CPU 使用率、文件描述符
- **内存趋势**: RSS 和 VSZ 的时间序列图
- **I/O 监控**: 读取/写入速率
- **上下文切换**: 自愿/非自愿切换速率
- **线程详情**: 线程数量、FD 使用率
- **缺页错误**: 次要/主要缺页错误速率

**可用变量**:
- `project_id`: 过滤项目 ID
- `instance`: 过滤实例
- `process_name`: 过滤进程名称
- `resolution`: 查询分辨率 (15s, 30s, 1m, 5m, 15m)

---

## ⚙️ 配置说明

### 核心配置文件

#### `docker-compose.yml`
监控服务配置，定义所有服务容器。

**服务列表**:
```yaml
services:
  rcoder:          # 主 RCoder 服务
  pyroscope:       # CPU 性能分析服务器
  prometheus:      # 时序指标数据库
  grafana:         # 可视化平台
```

#### `config.yml`
本地 Docker 容器测试专用配置，用于在 docker-compose 启动的容器中测试动态启动子容器。

### 镜像配置

所有容器使用相同的镜像，确保环境一致性：

```yaml
# 主容器和子容器使用相同镜像
image: "master-rcoder:latest"
```

### 路径配置

| 类型 | 容器内路径 | 宿主机映射路径 | 说明 |
|------|-----------|---------------|------|
| **项目工作目录** | `/app/project_workspace` | `./docker/project_workspace` | 项目代码存放 |
| **日志目录** | `/app/logs` | `./docker/logs` | 容器日志输出 |
| **规范目录** | `/app/specs` | - | 规范文件存放 |

### 与生产环境对比

| 配置项 | 本地测试 | 生产环境 |
|--------|---------|---------|
| **镜像** | `master-rcoder:latest` | `registry.yichamao.com/rcoder:latest-arm64` |
| **配置文件** | `docker/config.yml` | `config.yml` |
| **项目路径** | `/app/project_workspace` | `./project_workspace` |
| **监控服务** | 完整（Pyroscope + Prometheus + Grafana） | 按需部署 |

---

## 🔧 常用命令

### Make 命令

```bash
# 构建镜像
make dev-build

# 启动服务
make dev-up

# 查看日志
make dev-logs

# 重启服务（代码修改后）
make dev-restart

# 停止服务
make dev-down
```

### Docker 命令

```bash
# 查看运行中的容器
docker ps | grep rcoder

# 查看容器日志
docker logs -f <container_id>

# 进入容器
docker exec -it <container_id> bash

# 检查容器资源使用
docker stats <container_id>

# 检查挂载点
docker inspect <container_id> | grep Mounts -A 20
```

### API 测试

```bash
# 发送聊天请求（创建容器）
curl -X POST http://127.0.0.1:8088/computer/chat \
  -H "Content-Type: application/json" \
  -d '{"user_id": "user_123", "prompt": "hello"}'

# 查询 Agent 状态
curl http://127.0.0.1:8088/agent/status/user_123

# 取消会话
curl -X POST http://127.0.0.1:8088/agent/session/cancel \
  -H "Content-Type: application/json" \
  -d '{"session_id": "<session_id>"}'
```

---

## 💚 健康检查

### 快速验证所有服务

```bash
# 一键检查所有服务状态
curl -s http://localhost:4040/health  # Pyroscope
curl -s http://localhost:9091/-/healthy  # Prometheus
curl -s http://localhost:3000/api/health  # Grafana
curl -s http://localhost:8088/health  # RCoder
```

### 检查脚本

```bash
# 保存为 check-health.sh
#!/bin/bash
services=(
    "Pyroscope:4040:/health"
    "Prometheus:9091:/-/healthy"
    "Grafana:3000:/api/health"
    "RCoder:8088:/health"
)

for service in "${services[@]}"; do
    IFS=':' read -r name port endpoint <<< "$service"
    if curl -s "http://localhost:${port}${endpoint}" > /dev/null 2>&1; then
        echo "✅ $name (${port})"
    else
        echo "❌ $name (${port}) - 未响应"
    fi
done
```

### 监控数据验证

```bash
# 检查 Prometheus 是否接收指标
curl -s 'http://localhost:9091/api/v1/query?query=up' | jq '.data.result[]'

# 检查 Pyroscope 是否有应用数据
curl -s 'http://localhost:4040/ingest?name=agent_runner' | jq

# 检查 Grafana 数据源连接
curl -s 'http://admin:admin@localhost:3000/api/datasources' | jq '.[] | select(.name=="Prometheus") | .isDefault'
```

---

## 🧪 测试页面

`test-page/` 目录提供 VNC、音频和输入法透传功能的集成测试。

### 文件说明

| 文件 | 说明 |
|------|------|
| `vnc-test.html` | 集成测试页面（VNC + 音频 + IME） |
| `opus-decoder.min.js` | Opus 音频解码库（86KB） |

### 支持功能

- **VNC 远程桌面**: WebSocket 连接到容器的 noVNC 服务
- **音频流播放**: 接收并播放容器的音频输出（Opus 编码）
- **输入法透传**: 使用本地输入法输入到远程桌面

### 使用步骤

#### 1. 启动 HTTP 服务器

```bash
# 进入测试页面目录（从项目根目录执行）
cd docker/test-page

# 或者直接指定绝对路径
# cd $(git rev-parse --show-toplevel)/docker/test-page

# 使用 Python 启动服务器
python3 -m http.server 8000
```

#### 2. 访问测试页面

在浏览器打开：http://127.0.0.1:8000/vnc-test.html

#### 3. 配置连接参数

**推荐使用 RCoder 代理模式**:
- RCoder 服务地址: `http://127.0.0.1:8088`
- User ID: `user_123`
- Project ID: 留空或填写实际项目 ID

#### 4. 创建测试容器

```bash
curl -X POST http://127.0.0.1:8088/computer/chat \
  -H "Content-Type: application/json" \
  -d '{"user_id": "user_123", "prompt": "hello"}'
```

---

## 🔬 eBPF 诊断工具

### 概述

RCoder 集成 eBPF 诊断工具，用于开发环境中快速定位进程阻塞和性能问题。

> **详细文档**: `/docker/rcoder-agent-runner/ebpf-tools/README.md`

### ⚠️ 安全警告

| 模式 | Feature | 容器特权 | 安全性 | 调试能力 |
|------|---------|----------|--------|----------|
| `make dev-restart` | `ebpf-debug` 启用 | 特权 (SYS_ADMIN) | ⚠️ 降低 | ✅ 完整 |
| 生产模式 | 默认关闭 | 限制 | ✅ 高 | ❌ 无 |

> **仅在受信任的调试环境使用 eBPF 模式！**

### 监控能力

| 工具 | 类型 | 频率 | 输出位置 |
|------|------|------|----------|
| **Alloy eBPF** | 持续 CPU 监控 | 97 Hz | Pyroscope Web UI |
| **Alloy Process Exporter** | 进程指标 | 15 秒 | Grafana Dashboard |
| **offcpu-monitor** | 阻塞火焰图 | 60 秒 | `/app/container-logs/diag/*.svg` |
| **syscall-monitor** | 系统调用追踪 | 60 秒 | `/app/container-logs/diag/*.log` |

### 快捷诊断命令

```bash
# 1. 进入容器
docker exec -it <container> bash

# 2. 获取 agent_runner 进程 PID
PID=$(pgrep agent_runner)

# 3. 执行诊断命令
e-offcpu $PID       # CPU 性能分析（显示耗时函数）
e-flame $PID 60     # 生成 60 秒火焰图
e-profile $PID      # 性能分析
e-all $PID          # 综合诊断（包含所有分析）
```

### 导出诊断数据

```bash
# 导出所有诊断数据
docker cp <container>:/app/container-logs/diag ./diag-results

# 导出单个火焰图
docker cp <container>:/app/container-logs/diag/flame-<pid>.svg ./
```

---

## 🐛 故障排查

### 按症状分类

#### 症状：子容器无法启动

**可能原因**: 镜像不存在

```bash
# 检查镜像
docker images | grep master-rcoder

# 解决方案：重新构建
make dev-build
```

#### 症状：配置文件未生效

**可能原因**: 挂载路径错误

```bash
# 检查配置文件挂载
docker exec -it <container_id> cat /app/config.yml

# 检查日志
make dev-logs
```

#### 症状：监控服务无数据

**诊断步骤**:

```bash
# 1. 检查监控服务状态
docker ps | grep -E "pyroscope|prometheus|grafana"

# 2. 检查 agent_runner 容器
docker ps | grep agent_runner

# 3. 检查 Prometheus 指标
curl -s 'http://localhost:9091/api/v1/query?query=up' | jq

# 4. 检查 Alloy 日志
docker exec <container> tail -f /app/container-logs/diag/alloy.log
```

#### 症状：Grafana 显示 "No Data"

**可能原因**: 没有 agent_runner 容器运行

**解决方案**:
1. 创建容器（发送聊天请求）
2. 等待 15-30 秒让数据采集
3. 刷新 Dashboard

#### 症状：端口冲突

**Prometheus 端口冲突**（9090 → 9091）:

```yaml
# docker-compose.yml 已配置端口映射
prometheus:
  ports:
    - "9091:9090"  # 宿主机 9091 → 容器 9090
```

**检查端口占用**:

```bash
lsof -i :4040  # Pyroscope
lsof -i :9091  # Prometheus
lsof -i :3000  # Grafana
lsof -i :8088  # RCoder
```

### 系统化诊断流程

```mermaid
flowchart TD
    Start["🐛 问题发生"]
    Step1["1️⃣ 检查容器状态<br/>docker ps"]
    Step2["2️⃣ 查看容器日志<br/>docker logs"]
    Step3["3️⃣ 检查网络连通性<br/>docker network inspect"]
    Step4["4️⃣ 检查资源使用<br/>docker stats"]
    Step5["5️⃣ 进入容器调试<br/>docker exec"]
    Solve["✅ 问题解决"]

    Start --> Step1
    Step1 --> Step2
    Step2 --> Step3
    Step3 --> Step4
    Step4 --> Step5
    Step5 --> Solve

    style Start fill:#ffcdd2
    style Step1 fill:#e1f5fe
    style Step2 fill:#e1f5fe
    style Step3 fill:#e1f5fe
    style Step4 fill:#fff3e0
    style Step5 fill:#f3e5f5
    style Solve fill:#c8e6c9
```

---

## ❓ 常见问题

### Q1: 如何修改镜像名称？

编辑 `docker-compose.yml`:

```yaml
services:
  rcoder:
    image: "your-custom-image:tag"
```

然后运行 `make dev-restart`。

### Q2: 如何持久化监控数据？

编辑 `docker-compose.yml`，添加数据卷：

```yaml
services:
  prometheus:
    volumes:
      - ./prometheus/data:/prometheus
  grafana:
    volumes:
      - ./grafana/data:/var/lib/grafana
```

### Q3: 如何禁用某个监控服务？

注释掉 `docker-compose.yml` 中对应的服务配置。

### Q4: 如何调整资源限制？

编辑 `docker-compose.yml`:

```yaml
services:
  rcoder:
    deploy:
      resources:
        limits:
          cpus: '2'
          memory: 4G
```

### Q5: 容器间通信超时怎么办？

检查 Docker 网络：

```bash
# 查看网络
docker network ls

# 检查网络详情
docker network inspect rcoder_agent-network

# 测试连通性
docker exec <container1> ping <container2_ip>
```

### Q6: 如何清理所有容器和数据？

```bash
# 停止并删除所有容器
make dev-down

# 删除数据卷（谨慎操作）
docker volume prune

# 完全清理
docker system prune -a
```

---

## 📚 附录

### Prometheus 查询示例

```promql
# 内存使用趋势
process_resident_memory_bytes{project_id="user_123"}

# CPU 使用率
rate(process_cpu_seconds_total{project_id="user_123"}[30s]) * 100

# I/O 读取速率
rate(process_read_bytes_total{project_id="user_123"}[30s])

# 文件描述符使用率
process_open_fds{project_id="user_123"} / process_max_fds{project_id="user_123"}

# 上下文切换速率
rate(process_context_switches_total{project_id="user_123",context_switch_type="voluntary"}[30s])
```

### 目录结构

```
docker/
├── README.md                        # 本文档
├── config.yml                       # 容器内配置
├── docker-compose.yml               # 服务编排配置
├── computer-cache/                  # 计算机缓存
├── computer-project-workspace/      # 计算机项目工作区
├── grafana/                         # Grafana 配置
│   └── provisioning/                # 自动配置
│       ├── dashboards/              # Dashboard 定义
│       └── datasources/             # 数据源配置
├── logs/                            # 日志目录
├── project_workspace/               # 项目工作区
├── prometheus/                      # Prometheus 配置
│   └── prometheus.yml               # 规则文件
├── rcoder-agent-runner/             # Agent Runner 配置
│   └── ebpf-tools/                  # eBPF 工具
│       └── README.md                # 详细文档
├── rcoder-master/                   # 主服务配置
├── start-rcoder.sh                  # 启动脚本
└── test-page/                       # 测试页面
    ├── vnc-test.html                # VNC 测试页面
    └── opus-decoder.min.js          # Opus 解码库
```

> **注意**: `Make` 命令在项目根目录的 `Makefile` 中定义，使用 `make -C docker` 或从项目根目录执行。

### 相关文档

- [项目主文档](../README.md)
- [CLAUDE.md](../CLAUDE.md) - 项目架构和开发指南
- [eBPF 工具详细文档](./rcoder-agent-runner/ebpf-tools/README.md)
- [Makefile](../Makefile) - 构建命令说明

---

**最后更新**: 2026-01-13
**维护者**: RCoder Team

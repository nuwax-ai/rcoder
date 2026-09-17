# Docker 本地测试配置说明

> 本文档说明如何在本地使用 Docker Compose 启动 RCoder 开发环境，包含完整的观测服务（Tempo/Prometheus/Loki/Grafana）与 dial9 事件级 Tokio tracing（离线分析，零观测容器）。

## 📑 目录

- [快速开始](#-快速开始)
- [环境要求](#-环境要求)
- [架构概览](#-架构概览)
- [观测服务](#-观测服务)
- [配置说明](#-配置说明)
- [常用命令](#-常用命令)
- [测试页面](#-测试页面)
- [dial9 事件级 tracing](#-dial9-事件级-tokio-tracing)
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
| **Grafana** | http://localhost:3300 | 进程监控 Dashboard (admin/admin) |
| **Prometheus** | http://localhost:9091 | 时序指标查询 |

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
lsof -i :9091   # Prometheus
lsof -i :3300   # Grafana
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
        
        subgraph Monitoring["📊 观测服务"]
            direction LR
            Tempo["Tempo<br/>:3200"]
            Prom["Prometheus<br/>:9091"]
            Loki["Loki<br/>:3100"]
            Graf["Grafana<br/>:3300"]
        end
        
        RCoder ==>|"gRPC<br/>内部网络"| Runner
        RCoder -->|"OTLP"| Tempo
        Prom --> Graf
        Tempo --> Graf
        Loki --> Graf
    end
    
    style RCoder fill:#e1f5fe
    style Runner fill:#fff3e0
    style Tempo fill:#f3e5f5
    style Prom fill:#e8f5e9
    style Loki fill:#fffde7
    style Graf fill:#fce4ec
```

> **提示**: 在支持 Mermaid 的平台（GitHub、GitLab、IDE 插件）上，上图会渲染为交互式流程图。

### 数据流向

```mermaid
flowchart LR
    subgraph Apps["rcoder / agent_runner"]
        OTLP["OTLP traces"]
        LOGS["JSON 日志"]
        DIAL9["dial9 事件级 trace<br/>(DIAL9_ENABLED=1)"]
    end
    
    subgraph Pipeline["采集与存储"]
        COLLECTOR["otel-collector<br/>:4317"]
        TEMPO["Tempo<br/>trace 存储"]
        FB["fluent-bit"]
        LOKI["Loki<br/>日志检索"]
        PROM["Prometheus<br/>指标"]
    end
    
    subgraph Viz["可视化 / 分析"]
        GRAF["Grafana<br/>:3300"]
        VIEWER["dial9 serve<br/>离线 viewer"]
    end
    
    OTLP --> COLLECTOR --> TEMPO --> GRAF
    LOGS --> FB --> LOKI --> GRAF
    DIAL9 -->|"磁盘分段文件"| VIEWER
    
    style Apps fill:#e3f2fd
    style TEMPO fill:#f3e5f5
    style PROM fill:#e8f5e9
    style GRAF fill:#fce4ec
    style VIEWER fill:#fffde7
```

---

## 📊 观测服务

### 服务概览

| 服务 | 端口 | 登录信息 | 用途 |
|------|------|----------|------|
| **Tempo** | 3200 | — | OTLP 分布式追踪存储（经 otel-collector） |
| **Prometheus** | 9091 | 无需登录 | 时序指标（rcoder /metrics + collector 自身指标） |
| **Loki** | 3100 | 无需登录 | 日志检索（fluent-bit 采集） |
| **Grafana** | 3300 | admin / admin | 统一可视化（Tempo/Prometheus/Loki 数据源） |

Tokio 运行时层的深度剖析（poll/wake/task 时间线、调度延迟）由 dial9 离线承担，
详见 [docs/observability.md](../docs/observability.md) 与下方 dial9 节。

---

## ⚙️ 配置说明

### 核心配置文件

#### `docker-compose.yml`
观测服务配置，定义所有服务容器。

**服务列表**:
```yaml
services:
  rcoder:          # 主 RCoder 服务
  tempo:           # OTLP 分布式追踪存储
  otel-collector:  # 遥测采集管道
  prometheus:      # 时序指标数据库
  loki:            # 日志后端
  fluent-bit:      # 日志采集
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
| **观测服务** | 完整（Tempo + Prometheus + Loki + Grafana） | 按需部署 |

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
curl -s http://localhost:9091/-/healthy  # Prometheus
curl -s http://localhost:3300/api/health  # Grafana
curl -s http://localhost:8088/health  # RCoder
```

### 检查脚本

```bash
# 保存为 check-health.sh
#!/bin/bash
services=(
    "Prometheus:9091:/-/healthy"
    "Grafana:3300:/api/health"
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

# 检查 Grafana 数据源连接
curl -s 'http://admin:admin@localhost:3300/api/datasources' | jq '.[] | select(.name=="Prometheus") | .isDefault'
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

## 🔬 dial9 事件级 Tokio tracing

### 概述

dial9 经 Tokio runtime hooks 记录每个 poll/wake/task 事件到磁盘分段文件，
离线分析定位"这个 task 在等什么 / 这个 poll 为什么长"。已取代 tokio-console
与 Pyroscope/eBPF 持续剖析链。

### 使用方式

```bash
make dial9-on         # 启用记录（重建 rcoder 容器注入 DIAL9_ENABLED=1）
# ...复现场景...
make dial9-off        # 关闭（默认关，纯 passthrough 零开销）

# 离线查看（单二进制 viewer；首次先 cargo binstall dial9）
make dial9-view       # = dial9 serve --local-dir ./docker/logs/dial9
```

trace 落宿主 `docker/logs/dial9`（rcoder 主进程）/ `container-logs/dial9`
（agent 容器，bind 宿主可见）。变量表与行为要点见
[docs/observability.md](../docs/observability.md)。

### 容器内手动诊断

镜像保留 bpftrace/strace/sysstat/jq（需在 config.yml `services.security`
显式提权后使用）。

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

#### 症状：观测服务无数据

**诊断步骤**:

```bash
# 1. 检查观测服务状态
docker ps | grep -E "tempo|prometheus|grafana|loki"

# 2. 检查 agent_runner 容器
docker ps | grep agent_runner

# 3. 检查 Prometheus 指标
curl -s 'http://localhost:9091/api/v1/query?query=up' | jq
```

#### 症状：Grafana 显示 "No Data"

**可能原因**: 数据源无流量或查询时间窗不含数据

**解决方案**:
1. 确认对应后端（Tempo/Prometheus/Loki）容器运行中
2. 触发业务请求后刷新查询（Explore 选对应数据源）
3. 检查 otel-collector / fluent-bit 自身健康（:8888 / :2020）

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
lsof -i :9091  # Prometheus
lsof -i :3300  # Grafana
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

### Q3: 如何禁用某个观测服务？

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
- [可观测性指南](../docs/observability.md) - OTLP/Loki/dial9 使用说明
- [Makefile](../Makefile) - 构建命令说明

---

**最后更新**: 2026-01-13
**维护者**: RCoder Team

# eBPF 诊断工具

本目录包含用于监控和诊断 `agent_runner` 及其子进程性能的 eBPF 工具。

## 📊 监控架构总览

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         进程性能监控完整方案                              │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │                    CPU 性能监控（持续）                          │   │
│  │                                                                  │   │
│  │  Grafana Alloy (eBPF) → Pyroscope Server → Web UI (4040)       │   │
│  │  - 97 Hz 采样率                                                 │   │
│  │  - 每 15 秒发送数据                                              │   │
│  │  - 自动发现进程                                                  │   │
│  │  - 支持历史查询                                                  │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│                              ↓                                          │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │                  进程指标监控（持续）                            │   │
│  │                                                                  │   │
│  │  Alloy Process Exporter → Prometheus → Grafana Dashboard       │   │
│  │  - CPU、内存、I/O、FD、线程数                                     │   │
│  │  - 15 秒采集间隔                                                │   │
│  │  - 时序数据存储                                                  │   │
│  │  - Dashboard 可视化 (3000)                                       │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│                              ↓                                          │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │                  Off-CPU 阻塞监控（定期）                        │   │
│  │                                                                  │   │
│  │  offcputime-bpfcc → SVG 火焰图文件                               │   │
│  │  - 每 60 秒生成一次                                              │   │
│  │  - 显示阻塞堆栈                                                  │   │
│  │  - 识别 I/O、锁、等待等阻塞                                      │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│                              ↓                                          │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │                  系统调用监控（持续）                            │   │
│  │                                                                  │   │
│  │  syscount-bpfcc → 统计文件                                      │   │
│  │  execsnoop-bpfcc → 进程创建日志                                 │   │
│  │  opensnoop-bpfcc → 文件访问日志                                 │   │
│  │  - 每 60 秒统计一次                                             │   │
│  │  - 持续追踪进程和文件访问                                        │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│                              ↓                                          │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │                 手动诊断工具（按需使用）                          │   │
│  │                                                                  │   │
│  │  diag-tool.sh - 综合诊断工具                                     │   │
│  │  auto-flamegraph.sh - 自动火焰图生成                             │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

## 目录结构

```
ebpf-tools/
├── README.md              # 本文档
├── alloy-config.alloy     # Grafana Alloy 配置（CPU 监控 + 进程指标导出）
├── diag-tool.sh          # 手动诊断工具
├── auto-flamegraph.sh    # 自动火焰图生成
├── offcpu-monitor.sh     # Off-CPU 阻塞监控
└── syscall-monitor.sh    # 系统调用监控
```

---

## 🔧 工具详解

### 1. alloy-config.alloy - Grafana Alloy 配置

**用途**: 持续 CPU 性能监控 + 进程指标导出

**工作原理**:
- 使用 eBPF 自动发现 `agent_runner` 及其子进程
- 以 97 Hz 频率采样 CPU 性能数据，发送到 Pyroscope
- 采集进程指标（CPU、内存、I/O、FD、线程数），发送到 Prometheus
- 在 Web UI 中实时查看：Pyroscope (4040) + Grafana (3000)

**采集的指标**:
| 指标类别 | 指标名称 | 说明 |
|---------|---------|------|
| **CPU** | `process_cpu_seconds_total` | CPU 时间（累计） |
| **内存** | `process_resident_memory_bytes` | 常驻内存（RSS） |
| **内存** | `process_virtual_memory_bytes` | 虚拟内存（VSZ） |
| **FD** | `process_open_fds` | 打开的文件描述符 |
| **I/O** | `process_read_bytes_total` | 读取字节数 |
| **I/O** | `process_write_bytes_total` | 写入字节数 |
| **线程** | `process_num_threads` | 线程数量 |
| **上下文切换** | `process_context_switches_total` | 上下文切换次数 |

**进程标签**:
- `process_pid`: 进程 PID
- `process_name`: 进程名称
- `process_exe`: 完整执行路径
- `parent_pid`: 父进程 PID
- `project_id`: 项目 ID
- `container_id`: 容器 ID

**环境变量**:
| 变量 | 默认值 | 说明 |
|------|--------|------|
| `ENABLE_ALLOY` | - | 是否启用（由 ebpf-debug feature 控制） |
| `PYROSCOPE_URL` | http://pyroscope:4040 | Pyroscope Server 地址 |
| `PROMETHEUS_URL` | http://prometheus:9090/api/v1/write | Prometheus 写入地址 |

**查看数据**:
```bash
# 1. Pyroscope Web UI (CPU 性能)
open http://localhost:4040

# 2. Grafana Dashboard (进程指标)
open http://localhost:3000
# 登录: admin / admin
# 查找: "Agent Runner 进程监控"
```

---

### 2. offcpu-monitor.sh - Off-CPU 阻塞监控

**用途**: 定期生成 Off-CPU 阻塞火焰图，分析进程阻塞原因

**工作原理**:
- 每 60 秒自动运行一次（可配置）
- 对 `agent_runner` 及其所有子进程进行采样
- 使用 `offcputime-bpfcc` 捕获阻塞堆栈
- 生成 SVG 火焰图文件，自动清理旧文件（最多保留 50 个）

**环境变量**:
| 变量 | 默认值 | 说明 |
|------|--------|------|
| `ENABLE_OFFCPUTIME` | - | 是否启用（由 ebpf-debug feature 控制） |
| `OFFCPU_DURATION` | 30 | 每次采样时长（秒） |
| `OFFCPU_INTERVAL` | 60 | 生成间隔（秒） |
| `MAX_OFFCPU_FILES` | 50 | 最多保留文件数量 |

**输出文件**:
```
/app/container-logs/diag/
├── offcpu-monitor.log                           # 监控日志
├── offcpu-agent_runner-1-20250111_143025.svg   # 主进程阻塞火焰图
└── offcpu-claude-code-acp-123-*.svg            # 子进程阻塞火焰图
```

---

### 3. syscall-monitor.sh - 系统调用监控

**用途**: 监控进程的系统调用活动，包括进程创建、文件访问和系统调用统计

**工作原理**:
- 每 60 秒统计一次系统调用（可配置）
- 后台持续追踪进程创建 (`execsnoop-bpfcc`)
- 后台持续追踪文件访问 (`opensnoop-bpfcc`)
- 定期生成系统调用统计报告

**采集的数据**:
| 工具 | 数据 | 说明 |
|------|------|------|
| `syscount-bpfcc` | 系统调用统计 | 每个系统调用的次数和耗时 |
| `execsnoop-bpfcc` | 进程创建日志 | 新进程的创建时间和命令行 |
| `opensnoop-bpfcc` | 文件访问日志 | 文件打开/关闭操作 |

**环境变量**:
| 变量 | 默认值 | 说明 |
|------|--------|------|
| `ENABLE_SYSCALL_MONITOR` | - | 是否启用（由 ebpf-debug feature 控制） |
| `SAMPLE_DURATION` | 30 | 每次采样时长（秒） |
| `GENERATE_INTERVAL` | 60 | 生成间隔（秒） |

**输出文件**:
```
/app/container-logs/diag/
├── syscall-monitor.log                      # 监控日志
├── syscall-count-agent_runner-1-*.txt      # 系统调用统计
├── execsnoop-*.log                          # 进程创建日志
└── opensnoop-*.log                          # 文件访问日志
```

**日志说明**:
- **控制台输出**: 每次采样只输出一行汇总日志
- **文件日志**: 详细日志写入 `syscall-monitor.log`

---

### 4. diag-tool.sh - 手动诊断工具

手动触发的 eBPF 诊断工具，用于在怀疑有性能问题时主动采集数据。

**用法**:
```bash
diag-tool.sh {offcpu|flame|profile|all} <pid> [duration]
```

**命令**:
| 命令 | 说明 |
|------|------|
| `offcpu <pid> [duration]` | 分析 off-cpu 堆栈，默认 30 秒 |
| `flame <pid> [duration]` | 生成火焰图，默认 30 秒 |
| `profile <pid> [duration]` | CPU 性能分析，默认 30 秒 |
| `all <pid>` | 综合诊断（包含所有分析） |

**快捷命令**:
- `e-offcpu` - 等同于 `diag-tool.sh offcpu`
- `e-flame` - 等同于 `diag-tool.sh flame`
- `e-profile` - 等同于 `diag-tool.sh profile`
- `e-all` - 等同于 `diag-tool.sh all`

---

### 5. auto-flamegraph.sh - 自动火焰图生成

持续在后台运行的火焰图生成工具，自动监控 `agent_runner` 及其所有子进程。

**工作原理**:
1. 自动检测 `agent_runner` 进程 PID
2. 递归获取所有子进程 PID
3. 使用 bpftrace 采样性能数据（默认 30 秒）
4. 生成火焰图 SVG 文件
5. 每 60 秒重复一次

---

## 🎯 使用场景

### 场景 1: 持续监控 CPU 性能

**目标**: 在 Web UI 中实时查看 agent_runner 性能数据

**步骤**:
1. 确保容器已启动（Alloy 自动运行）
2. 打开 http://localhost:4040 (Pyroscope)
3. 选择应用 `agent_runner`
4. 按需过滤标签（如 `process_name="agent_runner"`）
5. 查看火焰图、时序数据、Top 函数

**适用问题**:
- CPU 使用率异常
- 函数调用热点分析
- 性能回归检测

---

### 场景 2: 查看进程指标趋势

**目标**: 监控内存、I/O、FD 等进程指标的趋势

**步骤**:
1. 打开 http://localhost:3000 (Grafana)
2. 登录（admin / admin）
3. 查找 "Agent Runner 进程监控" Dashboard
4. 选择 `project_id`、`instance`、`process_name` 过滤
5. 查看各面板数据

**可用面板**:
- 概览: RSS/VSZ 内存、CPU 使用率、文件描述符
- 内存趋势: RSS 和 VSZ 的时间序列图
- I/O 监控: 读取/写入速率
- 上下文切换: 自愿/非自愿切换速率
- 线程详情: 线程数量、FD 使用率
- 缺页错误: 次要/主要缺页错误速率

---

### 场景 3: 分析进程阻塞问题

**目标**: 找出进程为什么被阻塞（等待 I/O、锁等）

**步骤**:
1. 等待 offcpu-monitor 自动生成火焰图（或手动触发）
2. 导出 SVG 文件到本地
3. 在浏览器中打开火焰图
4. 查找宽的阻塞堆栈

```bash
# 导出最新的 Off-CPU 火焰图
docker cp <container>:/app/container-logs/diag/offcpu-*.svg ./
open offcpu-*.svg
```

**适用问题**:
- `new_session` 超时
- 进程响应缓慢
- I/O 阻塞
- 锁竞争

---

### 场景 4: 分析系统调用模式

**目标**: 了解进程的系统调用行为，找出系统调用热点

**步骤**:
1. 查看系统调用统计日志
2. 分析哪些系统调用最频繁
3. 查看 execsnoop/opensnoop 日志了解进程和文件访问

```bash
# 查看系统调用统计
docker exec <container> cat /app/container-logs/diag/syscall-count-*.txt | head -20

# 查看进程创建日志
docker exec <container> tail -f /app/container-logs/diag/execsnoop-*.log
```

---

### 场景 5: 手动诊断已知问题

```bash
# 进入容器
docker exec -it <container> bash

# 诊断 agent_runner
e-all $(pgrep agent_runner)

# 导出结果
docker cp <container>:/app/container-logs/diag ./diag-results
```

---

## 📈 火焰图分析

### CPU 火焰图（Alloy + Pyroscope）

**如何阅读**:
- **横轴**: CPU 时间占比（越宽表示占用越多）
- **纵轴**: 调用堆栈（从上到下是调用关系）
- **颜色**: 暖色调表示热点函数

**典型问题识别**:
| 现象 | 可能原因 |
|------|----------|
| 某个函数占据大部分宽度 | CPU 密集型计算 |
| 深层函数很宽 | 递归调用或深层嵌套 |
| 出现 `syscall` 大量时间 | 系统调用开销 |
| 出现 `sleep`/`usleep` | 主动休眠或等待 |

### Off-CPU 火焰图（offcputime-bpfcc）

**如何阅读**:
- **横轴**: 阻塞时间占比（越宽表示阻塞越久）
- **纵轴**: 阻塞时的调用堆栈
- **颜色**: 暖色调表示阻塞热点

**典型问题识别**:
| 现象 | 可能原因 |
|------|----------|
| `schedule()` 占据大量时间 | 进程被调度出去（CPU 竞争） |
| `do_wait()`/`wait_event()` | 等待事件或信号 |
| `__sock_sendmsg()`/`__sock_recvmsg()` | 网络 I/O 阻塞 |
| `blk_mq_submit_bio()` | 磁盘 I/O 阻塞 |
| `futex_wait()` | 锁等待（互斥锁） |

---

## 🔍 故障排查

### Grafana Dashboard 显示 "No Data"

```bash
# 1. 检查 Prometheus 中是否有数据
curl -s 'http://localhost:9091/api/v1/query?query=process_resident_memory_bytes'

# 2. 检查标签值
curl -s 'http://localhost:9091/api/v1/label/project_id/values'

# 3. 确认 agent_runner 容器正在运行
docker ps | grep agent_runner
```

### 变量下拉框为空

```bash
# 检查标签值是否存在
curl -s 'http://localhost:9091/api/v1/label/project_id/values'

# 如果没有数据，说明没有 agent_runner 容器在运行
# 启动一个 agent_runner 容器后再检查
```

### Pyroscope Web UI 无数据

```bash
# 1. 检查 Pyroscope Server
docker ps | grep pyroscope
docker logs rcoder-pyroscope

# 2. 检查 Alloy 是否发送数据
docker exec <container> grep "collected profiles" /app/container-logs/diag/alloy.log

# 3. 检查网络连接
docker exec <container> curl http://pyroscope:4040
```

### Off-CPU 火焰图未生成

```bash
# 1. 检查 offcputime-bpfcc 是否可用
docker exec <container> which offcputime-bpfcc

# 2. 检查监控进程
docker exec <container> ps aux | grep offcpu-monitor

# 3. 查看监控日志
docker exec <container> tail -f /app/container-logs/diag/offcpu-monitor.log
```

### 系统调用监控无输出

```bash
# 1. 检查 syscount-bpfcc 是否可用
docker exec <container> which syscount-bpfcc

# 2. 检查监控脚本是否运行
docker exec <container> ps aux | grep syscall-monitor

# 3. 查看监控日志
docker exec <container> tail -f /app/container-logs/diag/syscall-monitor.log
```

---

## ⚠️ 安全注意事项

⚠️ **eBPF 工具需要容器特权模式运行**，仅在受信任的调试环境使用！

生产环境请使用 `make docker-build-agent-production` 构建无 eBPF 工具的镜像。

---

## 📚 相关文档

- [Grafana Alloy Documentation](https://grafana.com/docs/alloy/latest/)
- [Pyroscope Documentation](https://pyroscope.io/docs/)
- [Prometheus Documentation](https://prometheus.io/docs/)
- [Grafana Documentation](https://grafana.com/docs/)
- [Brendan Gregg's FlameGraph](https://github.com/brendangregg/FlameGraph)
- [bpftrace 参考指南](https://bpftrace.dev/)
- 主项目文档: `/docker/README.md`

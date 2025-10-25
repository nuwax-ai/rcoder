# RCoder 调试工具使用指南

## 📖 概述

本指南旨在帮助开发者诊断和解决 RCoder 容器中的性能问题、阻塞问题和死锁问题。RCoder 是一个基于 Rust + Tokio 的异步应用，常见问题包括：

- 🚫 HTTP 请求阻塞或超时
- 🔒 DashMap 读写锁死锁
- ⚡ 单线程 Tokio 运行时阻塞
- 🐳 Docker API 调用长时间阻塞
- 🧹 清理任务无响应

## 🛠️ 工具安装

### 方法1：使用调试镜像（推荐）

```bash
# 构建调试版本镜像
docker build -f docker/Dockerfile.debug -t rcoder:debug .

# 在 docker-compose.yml 中替换镜像
image: rcoder:debug
```

### 方法2：在现有容器中安装

```bash
# 进入容器
docker exec -it docker-rcoder-1 bash

# 运行安装脚本
bash /app/install-debug-tools.sh

# 或手动安装核心工具
apt-get update && apt-get install -y \
    gdb strace lsof htop linux-perf \
    tcpdump netstat-nat ss
```

## 🚀 快速诊断流程

当遇到 RCoder 阻塞问题时，按以下步骤进行诊断：

### 1. 基础状态检查

```bash
# 快速分析进程状态
analyze-rcoder

# 专门诊断阻塞问题
diagnose-blocking

# 查看帮助信息
debug-help
```

### 2. 确认问题类型

```bash
# 检查容器健康状态
docker ps | grep rcoder

# 测试健康检查端点
curl -m 5 http://127.0.0.1:8086/health

# 查看最新日志
docker exec docker-rcoder-1 tail -50 /app/logs/rcoder.2025-*
```

### 3. 深入分析

根据初步检查结果，选择相应的分析工具：

- 📊 **性能问题** → 生成火焰图
- 🔒 **阻塞问题** → 系统调用追踪
- 🧵 **线程问题** → 线程状态分析
- 🌐 **网络问题** → 网络连接分析

## 🔥 性能分析工具

### 火焰图生成

火焰图是分析 CPU 性能瓶颈的最佳工具：

```bash
# 生成30秒的火焰图
generate-flamegraph 30

# 自定义输出文件
generate-flamegraph 60 /app/debug/blocking-analysis.svg

# 实时性能监控
perf top -p $(pgrep rcoder)
```

**火焰图分析要点：**
- 🔍 寻找宽度最大的函数调用（CPU 热点）
- 🎯 关注 `cleanup_task`、`docker_manager` 相关调用
- ⚠️ 查找 `futex_wait`、`poll` 等阻塞调用

### CPU 和内存分析

```bash
# CPU 使用统计
perf stat -p $(pgrep rcoder) -- sleep 10

# 内存使用详情
cat /proc/$(pgrep rcoder)/status | grep -E "(VmPeak|VmSize|VmRSS)"

# 内存映射分析
pmap $(pgrep rcoder)
```

## 🧵 线程和锁分析

### 线程状态检查

```bash
# 查看所有线程状态
for tid in $(ls /proc/$(pgrep rcoder)/task/); do
    echo "Thread $tid: $(cat /proc/$(pgrep rcoder)/task/$tid/wchan)"
done

# 交互式线程监控
htop -p $(pgrep rcoder)

# 线程详细信息
ps -L -p $(pgrep rcoder)
```

### DashMap 死锁诊断

**常见死锁模式：**

1. **读锁后写锁冲突**：
   ```bash
   # 查找 futex_wait_queue 的线程
   grep -l "futex_wait_queue" /proc/$(pgrep rcoder)/task/*/wchan
   ```

2. **长时间持有读锁**：
   ```bash
   # 监控锁竞争
   strace -p $(pgrep rcoder) -e trace=futex 2>&1 | head -20
   ```

**解决方案检查点：**
- ✅ 是否使用了 Entry API
- ✅ 是否及时释放了引用
- ✅ 是否避免了跨 await 点持有锁

## 🔍 系统调用追踪

### 基础追踪

```bash
# 追踪所有系统调用
strace -p $(pgrep rcoder)

# 只追踪网络相关调用
strace -p $(pgrep rcoder) -e trace=network

# 只追踪文件操作
strace -p $(pgrep rcoder) -e trace=file

# 统计系统调用
strace -p $(pgrep rcoder) -c
```

### 针对性追踪

```bash
# Docker API 调用追踪
strace -p $(pgrep rcoder) -e trace=connect,sendto,recvfrom

# 文件锁追踪
strace -p $(pgrep rcoder) -e trace=flock,fcntl

# 信号量和互斥锁
strace -p $(pgrep rcoder) -e trace=futex
```

## 🌐 网络诊断

### 连接状态分析

```bash
# 查看监听端口状态
ss -tulpn | grep $(pgrep rcoder)

# 检查积压队列
ss -tlnp | grep $(pgrep rcoder)

# 网络文件描述符
lsof -p $(pgrep rcoder) -i
```

### 网络阻塞问题

**队列积压检查：**
```bash
# 如果看到类似 "LISTEN 67 1024" 说明有积压
ss -tlnp | grep 8086
```

**解决方案：**
- 🔧 增加 backlog 大小
- ⚡ 优化请求处理速度
- 🧹 检查清理任务是否阻塞主线程

## 📝 日志分析

### 实时日志监控

```bash
# 实时查看日志
tail -f /app/logs/rcoder.2025-*

# 过滤错误和警告
tail -f /app/logs/rcoder.2025-* | grep -E "(ERROR|WARN|error|warn)"

# 搜索特定问题
grep -E "(阻塞|超时|timeout|blocking)" /app/logs/rcoder.2025-*
```

### 关键日志模式

**正常清理流程：**
```
🔥 [cleanup] 开始销毁Docker容器: project_id=xxx
🎯 [cleanup] 找到容器，开始销毁: project_id=xxx
✅ [cleanup] Docker容器销毁成功: project_id=xxx
```

**异常模式：**
```
🔥 [cleanup] 开始销毁Docker容器: project_id=xxx
# 然后没有后续日志 → 可能在容器查找或停止阶段卡住
```

## 🔧 Tokio 异步运行时分析

### Tokio Console（如果可用）

```bash
# 启用 tokio-console（需要重新编译）
TOKIO_CONSOLE_ENABLED=1 cargo run

# 连接到 console
tokio-console http://localhost:6669
```

### 手动异步分析

```bash
# 查看事件循环状态
cat /proc/$(pgrep rcoder)/task/*/wchan | sort | uniq -c

# 期望看到：
# - do_epoll_wait (正常的事件循环等待)
# - futex_wait_queue (可能的锁等待)
```

## 🐛 常见问题诊断

### 问题1：HTTP 请求超时

**症状：**
- curl 请求长时间无响应
- 健康检查失败
- 监听队列积压

**诊断步骤：**
```bash
# 1. 检查监听队列
ss -tlnp | grep 8086

# 2. 查看线程状态
analyze-rcoder

# 3. 追踪系统调用
strace -p $(pgrep rcoder) -e trace=accept,read,write
```

**常见原因：**
- 🧹 清理任务阻塞主线程
- 🔒 DashMap 死锁
- 🐳 Docker API 调用超时

### 问题2：清理任务阻塞

**症状：**
- 清理日志突然停止
- 容器状态一直是 "stopping"
- 后续清理任务无法执行

**诊断步骤：**
```bash
# 1. 检查最后的清理日志
grep "cleanup" /app/logs/rcoder.2025-* | tail -10

# 2. 查看 Docker API 调用
strace -p $(pgrep rcoder) -e trace=connect,sendto,recvfrom

# 3. 生成火焰图
generate-flamegraph 30
```

### 问题3：DashMap 死锁

**症状：**
- 大量线程处于 `futex_wait_queue` 状态
- 应用完全无响应
- CPU 使用率很低

**诊断步骤：**
```bash
# 1. 确认死锁模式
for tid in $(ls /proc/$(pgrep rcoder)/task/); do
    wchan=$(cat /proc/$(pgrep rcoder)/task/$tid/wchan)
    if [[ "$wchan" == "futex_wait_queue" ]]; then
        echo "Blocked thread: $tid"
    fi
done

# 2. 追踪 futex 调用
strace -p $(pgrep rcoder) -e trace=futex 2>&1 | head -20
```

## 🎯 问题解决流程

### 1. 立即缓解
```bash
# 重启容器（临时解决）
docker restart docker-rcoder-1
```

### 2. 问题分析
```bash
# 生成火焰图分析性能热点
generate-flamegraph 60

# 保存诊断信息
analyze-rcoder > /tmp/diagnosis-$(date +%Y%m%d-%H%M%S).log
```

### 3. 代码修复
基于分析结果，常见修复方向：

- **DashMap 优化**：使用 Entry API，避免长时间持锁
- **超时机制**：为 Docker API 调用添加超时
- **并发优化**：将阻塞操作移到独立线程池
- **错误处理**：改进错误恢复机制

### 4. 验证修复
```bash
# 重新构建和部署
cargo build --release
docker-compose up -d

# 压力测试
for i in {1..100}; do
    curl -m 1 http://127.0.0.1:8086/health &
done
wait
```

## 📊 性能监控建议

### 生产环境监控

```bash
# 定期生成火焰图
*/30 * * * * generate-flamegraph 10 /app/debug/perf-$(date +\%H\%M).svg

# 监控队列积压
*/5 * * * * ss -tlnp | grep 8086 >> /app/logs/queue-monitor.log

# 内存使用趋势
*/10 * * * * cat /proc/$(pgrep rcoder)/status | grep VmRSS >> /app/logs/memory.log
```

### 关键指标

- 🌐 **网络队列积压**：应该 < 10
- 🧵 **阻塞线程数**：应该 = 0
- 💾 **内存使用**：应该稳定，无泄漏
- ⚡ **响应时间**：健康检查 < 100ms

## 🔗 相关资源

- [Rust Performance Book](https://nnethercote.github.io/perf-book/)
- [DashMap Documentation](https://docs.rs/dashmap/)
- [Tokio Performance Guide](https://tokio.rs/tokio/topics/performance)
- [FlameGraph Tutorial](http://www.brendangregg.com/flamegraphs.html)

---

> 💡 **小贴士**：定期运行 `analyze-rcoder` 和 `diagnose-blocking` 可以帮助提前发现潜在问题。在生产环境中，建议启用详细日志记录并定期分析性能指标。
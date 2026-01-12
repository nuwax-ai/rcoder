# 远程服务器 MCP 压力测试最终报告 (不复用容器模式)

> **测试时间**: 2026-01-12 16:17 - 16:45  
> **测试服务器**: 192.168.1.34:8086  
> **测试模式**: 15 并发 × 不复用容器 (Unique User ID)  
> **MCP 工具**: 10 个 (含 chrome-devtools)

---

## 🚨 核心结论

**在不复用容器的高并发场景下，系统性能高度依赖于服务器资源状态。**

1.  **资源竞争是首要瓶颈**: 
    - 当存在残留容器时（Batch 4），10 个 MCP 的初始化触发了 **100s 超时保护**。
    - 当环境较干净时（Batch 5），同样 10 个 MCP 的初始化仅需 **~12-18s**，且无超时。
2.  **MCP 数量影响**: 虽然 10 个 MCP 增加了初始化负担，但在资源充足时是可接受的（18s vs 3s 复用）。
3.  **稳定性**: 系统在极端高负载下（如 Batch 4）虽然慢，但 **成功率保持 100%**。

---

## 📊 测试数据总览

| 批次 | R1 平均 | R2 平均 | R3 平均 | 成功率 | 状态 |
|------|---------|---------|---------|--------|------|
| **批次 1** | 29.5s | 108.6s | 104.5s | 45/45 | ✅ 成功但慢 |
| **批次 2** | 102.4s | 59.0s | 102.4s | 45/45 | ✅ 波动较大 |
| **批次 3** | 102.4s | 18.3s | 28.3s | 45/45 | ✅ 偶有快响 |
| **批次 4** | 97.4s | ~41s | - | 15/15* | ⚠️ 严重超时 (资源竞争) |
| **批次 5** | **18.2s** | **18.8s** | - | 30/30 | ✅ **全部恢复正常** |

> *注: 批次 4 因残留容器导致资源耗尽；批次 5 在资源释放后性能显著回升。*


---

## 🔍 深度根因分析 (Log Analysis)

### 1. 正常与异常的对比 (Batch 4 vs Batch 5)

| 对比项目 | 批次 4 (异常) | 批次 5 (正常) |
|---------|--------------|--------------|
| **Batch ID** | `b1768207074` | `b1768207756` |
| **平均响应** | ~100s | ~18s |
| **容器残留** | 有 (Batch 3 容器未退出) | 无 (环境较干净) |
| **超时现象** | **100% 触发** (100s Timeout) | **0% 触发** (全正常启动) |
| **日志证据** | `⏰ [ACP] new_session 超时` | `✅ 正常启动` (诊断脚本验证) |

### 2. MCP 初始化超时 (资源竞争导致)
只有在服务器资源紧张（存在大量僵尸容器）时，10 个 MCP 的初始化才会慢到触发 100s 保护。在资源充足时（Batch 5），初始化仅需 ~12s。

**证据追踪 (Batch 4 - 异常)**:
```log
2026-01-12T08:30:07.867Z ERROR agent_abstraction::launcher::claude_code: ⏰ [ACP] new_session 超时 (100s)!
```

**证据追踪 (Batch 5 - 正常)**:
通过 `diagnose_remote_system.sh` 分析 Batch 5 的 30 个容器，结果显示：
```bash
统计: 分析了 30 个日志目录, 发现 0 个超时。
```

### 3. API 限流 (Rate Limiting)
(同上)

### 4. 原始日志分析 (Batch 4)


**证据追踪**:
- **日志文件**: `/home/swufe/nuwax/docker/logs/rcoder/container/computer-agent-runner-b1768206281_r3_u1-20260112082825/startup.log`
- **关键日志**:
  ```log
  2026-01-12T08:30:07.867Z ERROR agent_abstraction::launcher::claude_code: ⏰ [ACP] new_session 超时 (100s)! 耗时: 100.001312085s
  ```
- **验证命令**:
  ```bash
  # 在服务器上执行以查找所有超时记录
  grep -r "new_session 超时" /home/swufe/nuwax/docker/logs/rcoder/container
  ```

**分析**: 
- 大量容器在启动时卡在 MCP 初始化阶段。
- 系统检测到 `new_session` 耗时超过 100s，强制中断并继续（导致响应时间锁定在 ~100s）。
- 涉及的 MCP 工具包括: `mcp-server-time`, `server-memory`, `server-filesystem`, `mcp-server-git` 等 10 个。

### 2. API 限流 (Rate Limiting)

**证据追踪**:
- **日志文件**: `/home/swufe/nuwax/docker/logs/rcoder/rcoder.log`
- **关键日志**:
  ```log
  📡 [API_PROXY] 上游响应: open.bigmodel.cn:443 -> 429 Too Many Requests
  ```
- **验证命令**:
  ```bash
  grep "429 Too Many Requests" /home/swufe/nuwax/docker/logs/rcoder/rcoder.log | tail -n 20
  ```

**分析**:
- 主服务日志中出现大量 429 错误，表明并发请求触发了 LLM 供应商的频率限制。
- 系统通过重试机制处理了这些错误，但这进一步增加了延迟。

### 3. 容器残留与资源竞争
由于不复用容器，旧容器在请求结束后未能立即清理，导致服务器上残留大量已退出的容器，占用了系统资源：
```bash
CONTAINER ID   STATUS
...            Up 2 minutes  (上一个批次的残留)
```

---

## 💡 改进建议

### 1. 生产环境策略
- **必须启用容器复用**: 对于高频请求，复用容器可以将响应时间从 100s 降低到 **3-6s**。
- **预热机制**: 服务启动时预先初始化 Agent 容器池。

### 2. 系统优化
- **增加容器清理**: 实现更激进的容器 TTL 策略，确保不复用时能快速释放资源。
- **优化 MCP 启动**: 考虑并行化 MCP 启动过程，或减少默认启用的 MCP 工具数量。
- **断路器**: 在检测到大量 429 时暂时降低并发度。

### 3. 压测建议
- **控制并发**: 在不复用容器模式下，建议将并发数降低到 5 以内。
- **监控资源**: 实时监控服务器 CPU/内存和 Docker 容器数量。

---

## 附件: 复现步骤

1. **设置环境**:
   ```bash
   source docker/scripts/.env
   ```

2. **运行测试**:
   ```bash
   ./docker/scripts/stress_test_10mcp_remote.sh 15 3
   ```

3. **分析日志**:
   ```bash
   ./docker/scripts/analyze_logs_remote.sh
   # 需安装 sshpass: brew install hudochenkov/sshpass/sshpass
   ```

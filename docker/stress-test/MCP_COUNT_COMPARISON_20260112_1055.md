# MCP 工具数量压力测试对比报告

> **测试时间**: 2026-01-12 10:55  
> **测试配置**: 15 并发 × 1 轮  
> **容器状态**: 测试前已清理

---

## 测试结果对比

| 场景 | MCP 工具数 | 平均耗时 | 最快 | 最慢 | 成功率 | 状态 |
|------|-----------|----------|------|------|--------|------|
| **默认配置** | 1 (trends-hub) | **8.1s** | 7.2s | 9.2s | 100% | ✅ 正常 |
| **10 MCP 工具** | 10 (不同工具) | **66.2s** | 59.3s | 72.5s | 100% | ⚠️ 缓慢 |
| **差异** | +9 | **+58s (8x)** | - | - | - | - |

---

## 详细数据

### 场景 A: 默认配置 (1 MCP 工具)
**工具**: trends-hub (via mcp-proxy)

```
✅ R1-1:  9.22s
✅ R1-2:  8.98s
✅ R1-3:  8.79s
✅ R1-4:  8.62s
✅ R1-5:  8.38s
✅ R1-6:  8.18s
✅ R1-7:  8.00s
✅ R1-8:  7.80s
✅ R1-9:  7.59s
✅ R1-10: 7.25s
✅ R1-11: 8.09s
✅ R1-12: 8.08s
✅ R1-13: 8.20s
✅ R1-14: 7.62s
✅ R1-15: 7.33s
平均: 8.1s
```

### 场景 B: 10 MCP 工具
**工具列表**:
1. mcp-server-time (uvx)
2. mcp-server-fetch (uvx)
3. server-memory (npx)
4. server-filesystem (npx)
5. mcp-server-git (uvx)
6. server-github (npx)
7. mcp-server-sqlite (uvx)
8. server-puppeteer (npx)
9. server-brave-search (npx)
10. server-sequential-thinking (npx)

```
✅ R1-1:  60.90s
✅ R1-2:  64.30s
✅ R1-3:  68.65s
✅ R1-4:  72.47s  ← 最慢
✅ R1-5:  66.05s
✅ R1-6:  66.58s
✅ R1-7:  70.21s
✅ R1-8:  70.76s
✅ R1-9:  69.53s
✅ R1-10: 65.15s
✅ R1-11: 65.48s
✅ R1-12: 59.28s  ← 最快
✅ R1-13: 63.35s
✅ R1-14: 66.63s
✅ R1-15: 68.29s
平均: 66.2s
```

---

## 分析

### 🔍 关键发现

1. **线性增长**: MCP 工具数量从 1 增加到 10，初始化时间增加约 **8 倍**。
2. **无阻塞**: 即使 10 个工具，所有 15 个请求都在 72s 内完成，未触发 100s 超时。
3. **工具类型影响**: 
   - `uvx` 工具 (time, fetch, git, sqlite) 相对较快
   - `npx` 工具 (memory, filesystem, github, puppeteer, brave-search, sequential-thinking) 需要额外下载

### ⚠️ 性能瓶颈

| 因素 | 影响 |
|------|------|
| MCP 并行初始化 | 多个工具同时启动，争抢 CPU/网络 |
| npx 包下载 | 未预安装的包需要运行时下载 |
| 进程数量 | 10 个 MCP = 10 个子进程 |

### 💡 优化建议

1. **减少 MCP 工具数**: 生产环境建议 ≤ 3 个
2. **预安装**: 将常用 npx 包写入 Dockerfile
3. **使用 uvx**: uvx 工具通常比 npx 快（利用缓存）
4. **串行初始化**: 考虑按需加载而非并行初始化

---

## 复现脚本

| 场景 | 脚本 |
|------|------|
| 默认 (1 MCP) | `stress_test_mcp.sh 15 1` |
| 10 MCP 工具 | `stress_test_10mcp.sh 15 1` |

**运行方式**:
```bash
source docker/scripts/.env
./docker/scripts/stress_test_mcp.sh 15 1
./docker/scripts/stress_test_10mcp.sh 15 1
```

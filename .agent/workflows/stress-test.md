---
description: MCP 压力测试工作流 - 针对远程 rcoder 服务进行 MCP 并发压测
---

# MCP 压力测试工作流

本工作流用于对远程 rcoder 服务进行 MCP 并发压力测试，并生成详细的测试报告。

## 前置条件

1. 确保已安装 `sshpass` 工具：

```bash
// turbo
brew install hudochenkov/sshpass/sshpass
```

2. 确保 `.env` 配置文件已正确设置（位于 `docker/scripts/.env`）：

```bash
# 远程服务器配置
REMOTE_HOST="192.168.1.69"     # 目标服务器 IP
REMOTE_USER="lenovo"            # SSH 用户名
REMOTE_PASS="xxxxx"             # SSH 密码
REMOTE_API_PORT="9086"          # API 端口

# API Key (必须设置)
TEST_API_KEY="your_api_key_here"

# 日志路径配置
export MAIN_LOG_PATH=/path/to/logs/rcoder/rcoder.log
export CONTAINER_LOG_DIR=/path/to/logs/rcoder/container
```

---

## 步骤 1: 准备测试环境

检查远程服务器状态，确保服务正常运行：

```bash
// turbo
cd /Users/apple/workspace/rcoder/docker/scripts && ./diagnose_remote_system.sh
```

预期输出应显示：

- ✅ 运行中的子容器数量（建议 < 20）
- ✅ 无 MCP 初始化超时
- ✅ 无 API 限流错误

---

## 步骤 2: 运行压力测试

执行详细压测脚本，参数说明：

- 参数1: 并发数（默认 10）
- 参数2: 轮次（默认 4）
- 参数3: 输出日志文件（可选）

```bash
cd /Users/apple/workspace/rcoder/docker/scripts && ./stress_test_10mcp_remote_detailed.sh 10 4 /tmp/stress_test_$(date +%Y%m%d_%H%M%S).log
```

测试配置说明：
| 参数 | 推荐值 | 说明 |
|------|--------|------|
| 并发数 | 10 | 同时发起的请求数 |
| 轮次 | 4 | 每个批次的测试轮数 |
| MCP 数量 | 10 | 每个请求启用的 MCP 工具数 |

测试过程中会显示每个请求的实时状态：

- ✅ 表示请求成功
- ⚠️ 表示接近超时（> 100s）
- ❌ 表示请求失败

---

## 步骤 3: 分析测试结果

使用诊断脚本分析特定批次的测试结果：

```bash
cd /Users/apple/workspace/rcoder/docker/scripts && ./diagnose_remote_system.sh <BATCH_ID>
```

> **注意**: BATCH_ID 会在压测脚本开始时显示，格式如 `b1768211265`

诊断脚本会检查：

1. **[ACP] new_session 超时** - MCP 初始化超时情况
2. **智能体初始化超时** - Agent 启动超时情况
3. **容器状态** - 运行中和已退出的容器信息
4. **日志分析** - 错误和异常日志

---

## 步骤 4: 生成测试报告

基于测试结果，创建标准化测试报告。报告模板：

```markdown
# 远程 MCP 压测报告

> **开始时间**: YYYY-MM-DD HH:MM:SS  
> **测试服务器**: <HOST>:<PORT>  
> **模式**: <并发数> 并发 × <模式描述>  
> **测试计划**: <批次数> 个批次，每个批次 <轮次> 轮，每轮 <请求数> 个请求

## 📊 批次汇总

| 批次 | Batch ID | 启动时间 | R1 平均 | R2 平均 | ... | 超时数 | 状态 |
| ---- | -------- | -------- | ------- | ------- | --- | ------ | ---- |

## 📝 详细记录

### Batch N

- **ID**: `bXXXX`
- **MCP 状态**: 各轮容器启动情况
- **性能分析**: 各轮平均响应时间和性能变化
- **[ACP] 超时**: 超时数量及分析

## 🔍 请求粒度详情

### Batch N 请求详情

| 请求ID | User ID | 耗时(s) | HTTP状态 | 是否超时 |
| ------ | ------- | ------- | -------- | -------- |

## 📈 性能分析

### 关键发现

1. 性能趋势分析
2. 瓶颈识别
3. 资源竞争情况

### 结论与建议

1. 优化建议
2. 配置调整推荐
```

---

## 常用测试场景

> [!TIP]
> 压测脚本已集成自动清理逻辑 (`cleanup.sh`)，每次执行前会自动清理容器以保证基准一致。

### 场景 A: claude-code-acp-ts 本地压测（首选）

```bash
# 1. 执行压测 (10 MCP) - 自动清理环境
cd 20260124
// turbo
./stress_test_acp_ts_local.sh 10 2 /tmp/acp_ts_local.log

# 2. 生成报告
cd .. && ./generate_report.sh /tmp/acp_ts_local.log
```

### 场景 B: nuwaxcode 本地压测

```bash
# 1. 执行压测 (10 MCP) - 自动清理环境
cd 20260124
./stress_test_nuwaxcode_local.sh 10 2 /tmp/nuwax_local.log

# 2. 生成报告
cd .. && ./generate_report.sh /tmp/nuwax_local.log
```

### 场景 C: claude-code-acp-ts 远程压测

```bash
# 1. 执行压测 - 自动清理环境
cd 20260124
./stress_test_acp_ts_remote.sh 10 2 /tmp/acp_ts_remote.log

# 2. 生成报告
cd .. && ./generate_report.sh /tmp/acp_ts_remote.log
```

### 场景 D: nuwaxcode 远程压测

```bash
# 1. 执行压测 - 自动清理环境
cd 20260124
./stress_test_nuwaxcode_remote.sh 10 2 /tmp/nuwax_remote.log

# 2. 生成报告
cd .. && ./generate_report.sh /tmp/nuwax_remote.log
```

### 场景 E: 容器复用测试（相同 user_id）

修改脚本中的 `user_id` 逻辑，使用固定值以测试容器复用性能。

---

## 性能基准参考

| 指标             | 正常范围 | 预警阈值 | 临界阈值 |
| ---------------- | -------- | -------- | -------- |
| 冷启动响应时间   | 10-15s   | > 30s    | > 60s    |
| 容器复用响应时间 | 3-6s     | > 10s    | > 30s    |
| 运行中容器数     | 0-20     | > 30     | > 50     |
| [ACP] 超时率     | 0%       | > 5%     | > 10%    |

---

## 故障排查

### 问题 1: 大量 [ACP] new_session 超时

**可能原因**: MCP 工具启动时间过长，资源竞争激烈
**解决方案**:

- 减少启用的 MCP 工具数量
- 增加 MCP 初始化超时时间
- 检查 MCP 服务依赖（如 npx, uvx 缓存）

### 问题 2: 性能逐轮下降

**可能原因**: 容器未及时清理，资源累积
**解决方案**:

- 优化容器清理策略
- 增加批次间等待时间
- 启用容器复用模式

### 问题 3: 429 Too Many Requests

**可能原因**: LLM API 限流
**解决方案**:

- 降低并发数
- 更换更高配额的 API Key
- 增加请求间隔

---

## 相关文件

- **压测脚本目录**: `docker/scripts/20260124/`
  - `stress_test_acp_ts_local.sh` - claude-code-acp-ts 本地压测
  - `stress_test_acp_ts_remote.sh` - claude-code-acp-ts 远程压测
  - `stress_test_nuwaxcode_local.sh` - nuwaxcode 本地压测
  - `stress_test_nuwaxcode_remote.sh` - nuwaxcode 远程压测
- **工具脚本**:
  - `docker/scripts/generate_report.sh` - 压测报告生成脚本
  - `docker/scripts/timed_dev_restart.sh` - 带耗时记录的构建脚本
  - `docker/scripts/cleanup.sh` - 容器清理脚本
- 诊断脚本: `docker/scripts/diagnose_remote_system.sh`
- 环境配置: `docker/scripts/.env`

# 远程服务器 MCP 压力测试方案

> **测试时间**: 2026-01-12 15:37  
> **测试服务器**: 192.168.1.34:8086  
> **测试配置**: 15 并发 × 3 轮  
> **启动类型**: 远程服务器测试

---

## 测试目标

验证远程服务器在 10 个 MCP 工具配置下的并发处理能力和稳定性。

---

## 测试配置

### 服务器信息

| 项目 | 值 |
|------|------|
| 配置文件 | `docker/scripts/.env` |
| 说明 | 服务器地址、账号、密码等信息请参考 `.env` 文件 |

### MCP 工具配置 (10个)

| # | 工具名称 | 类型 | 说明 |
|---|---------|------|------|
| 1 | **chrome-devtools** | mcp-proxy | 浏览器自动化 ★默认 |
| 2 | mcp-server-time | uvx | 时间服务 |
| 3 | mcp-server-fetch | uvx | HTTP 请求 |
| 4 | server-memory | npx | 内存 KV 存储 |
| 5 | server-filesystem | npx | 文件系统访问 |
| 6 | mcp-server-git | uvx | Git 操作 |
| 7 | server-github | npx | GitHub API |
| 8 | mcp-server-sqlite | uvx | SQLite 数据库 |
| 9 | server-brave-search | npx | 搜索引擎 |
| 10 | server-sequential-thinking | npx | 思维链 |

---

## 测试步骤

### 步骤 1: 设置环境变量

```bash
export TEST_API_KEY="your_zhipu_api_key"
```

### 步骤 2: 重启远程主服务 (可选)

```bash
./docker/scripts/restart_remote_service.sh
```

或手动执行:
```bash
ssh swufe@192.168.1.34
# 密码: Swufe@2024
docker restart d5cf116c863a
```

### 步骤 3: 运行压测

```bash
./docker/scripts/stress_test_10mcp_remote.sh 15 3
```

### 步骤 4: 分析日志

```bash
./docker/scripts/analyze_logs_remote.sh
```

---

## 预期结果模板

### 第 1 轮
```
R1 平均: __s
最快: __s
最慢: __s
成功: __/15
```

### 第 2 轮
```
R2 平均: __s
最快: __s
最慢: __s
成功: __/15
```

### 第 3 轮
```
R3 平均: __s
最快: __s
最慢: __s
成功: __/15
```

---

## 监控项

| 监控项 | 命令 |
|--------|------|
| 查看容器状态 | `docker ps --filter "name=computer-agent"` |
| 查看实时日志 | `tail -f /var/log/rcoder/rcoder*.log` |
| 检查资源使用 | `docker stats` |

---

## 脚本清单

| 脚本 | 功能 | 用法 |
|------|------|------|
| `stress_test_10mcp_remote.sh` | 10 MCP 压测 | `./stress_test_10mcp_remote.sh 15 3` |
| `analyze_logs_remote.sh` | 远程日志分析 | `./analyze_logs_remote.sh` |
| `restart_remote_service.sh` | 重启主服务 | `./restart_remote_service.sh` |

---

## 注意事项

1. 确保 `TEST_API_KEY` 已设置
2. 远程服务器需要开放 8086 端口
3. 测试前建议重启主服务确保干净环境
4. 每轮间隔 5 秒，观察资源恢复情况

---

## 结果记录

> 待测试完成后填写...

| 指标 | 第1轮 | 第2轮 | 第3轮 |
|------|-------|-------|-------|
| 平均响应时间 | - | - | - |
| 成功率 | - | - | - |
| 状态 | - | - | - |

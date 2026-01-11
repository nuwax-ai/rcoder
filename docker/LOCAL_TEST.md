# RCoder 本地容器测试指南

## 快速启动

```bash
# 构建并启动
make dev-restart

# 仅启动（已构建）
make dev-up

# 停止服务
make dev-down

# 查看日志
make dev-logs

# 验证服务
curl http://localhost:8087/health
```

---

## 环境变量

测试脚本需要设置 API Key：
```bash
export TEST_API_KEY=your_api_key_here
```

---

## 测试脚本

| 脚本 | 用途 | 命令 |
|------|------|------|
| 单请求 | 基本功能验证 | `./docker/scripts/test_single.sh` |
| 并发测试 | 8 个并发 | `./docker/scripts/test_concurrent.sh` |
| 压力测试 | N并发×M轮 | `./docker/scripts/stress_test.sh 15 3` |
| 日志分析 | 定位问题 | `./docker/scripts/analyze_logs.sh` |
| **清理容器** | 清理压测容器 | `./docker/scripts/cleanup.sh` |

---

## 容器管理

```bash
# 查看运行容器数
docker ps --filter "name=computer-agent" | wc -l

# 查看资源使用
docker stats --no-stream

# 清理测试容器
docker ps -a --filter "name=computer-agent" -q | xargs -r docker rm -f
```

---

## 日志分析

```bash
# 实时跟踪关键日志
docker logs -f rcoder-rcoder-1 2>&1 | grep -E "(接收|创建|new_session|超时)"

# 查看最近错误
docker logs rcoder-rcoder-1 2>&1 | grep -E "(ERROR|超时|失败)" | tail -20
```

**关键日志关键字**:
| 关键字 | 含义 | 代码位置 |
|--------|------|----------|
| `agent_worker 接收到新请求` | 请求入口 | `acp_agent.rs:190` |
| `创建 ACP 会话[new_session]` | 创建会话 | `claude_code_agent.rs` |
| `new_session 超时` | **阻塞点** | `claude_code_agent.rs` |

---

## 压测结果参考

| 并发数 | 轮次 | 平均耗时 | 状态 |
|--------|------|----------|------|
| 10 | 1 | ~3s | ✅ 正常 |
| 10 | 2 | ~4s | ✅ 正常 |
| 15 | 1 | ~3s | ✅ 正常 |
| 15 | 3 | ~100s | ⚠️ 资源耗尽 |

> ⚠️ **注意**: 高并发会累积容器导致内存不足，测试后及时清理

---

## 目录结构
```
docker/
├── config.yml              # 容器配置
├── docker-compose.yml      # Docker Compose
├── LOCAL_TEST.md           # 本文档
├── project_workspace/      # 项目工作目录
├── logs/                   # 日志目录
└── scripts/
    ├── test_single.sh      # 单请求测试
    ├── test_concurrent.sh  # 并发测试
    ├── stress_test.sh      # 压力测试
    └── analyze_logs.sh     # 日志分析
```

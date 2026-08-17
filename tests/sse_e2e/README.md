# SSE 消息链路集成测试

黑盒测试 `/computer/chat` + `/computer/progress/{session_id}` 的轮次语义。

## 准备

1. 本地 dev 容器跑着最新代码（`make dev-hot`；agent_runner 改动需重建/替换镜像）
2. 仓库根 `.env.local`（已被 gitignore）：

```
LLM_API_KEY=sk-xxx
LLM_BASE_URL=https://api.deepseek.com
LLM_MODEL=deepseek-v4-flash
```

## 运行

```bash
python3 tests/sse_e2e/run.py              # 全部 5 场景
python3 tests/sse_e2e/run.py reconnect    # 按名过滤（子串）
```

## 场景语义

| 场景 | 断言 |
|---|---|
| full_turn_delivery | chat 后立刻连 SSE：完整事件链 + id 行单调无重复 |
| after_terminal_empty | turn 结束后连 SSE：0 事件（终端即清） |
| two_turn_isolation | 第二轮流 seq 全 > 第一轮最大；恰一个 prompt_start/end_turn |
| reconnect_with_cursor | 断开带 Last-Event-ID 重连：只收 seq > 游标的增量 |
| reconnect_no_cursor | 断开无游标重连：全量重放本轮（含本轮开头） |

结果输出 `results/<时间戳>/`（summary.txt + 每场景 json 明细）。
每场景独立 user（独立 agent 容器）——同 user 的 agent 是 prompt 串行的，
场景间不隔离会被 cancel 干扰。

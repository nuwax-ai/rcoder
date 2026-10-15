# SSE 消息链路集成测试

黑盒测试 `/computer/chat` + `/computer/progress/{session_id}` 的轮次语义。

## 准备

1. 本地 dev 容器跑着最新代码（`make dev-hot`；agent_runner 改动需重建/替换镜像）
2. 仓库根 `.env.local`（已被 gitignore）：

```
LLM_API_KEY=your-api-key
LLM_BASE_URL=https://api.deepseek.com
LLM_MODEL=your-model-id
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

## K8s 多入口专项

`lb_test.py` 需要显式提供 `TEST_K8S_SSH` 和 `LB_ENTRY_HOSTS`，缺少任一项会在 HTTP/SSH 操作前退出。先配置自己的获准测试集群，再运行：

```bash
TEST_K8S_SSH=your-cluster-alias \
  LB_ENTRY_HOSTS=node-a.example.com,node-b.example.com,node-c.example.com \
  LB_NODEPORT=30295 \
  python3 tests/sse_e2e/lb_test.py
```

`LB_ENTRY_HOSTS` 为逗号分隔的节点 IP 或主机名，不包含协议和端口。单入口仍可验证会话与 SSE 语义，不能作为多节点负载均衡证据。集群部署和资源边界见 [Kubernetes 指南](../../docs/deployment/kubernetes.md)。真实地址与凭据只保存在本地环境文件中；运行时将上述变量显式传入命令环境。

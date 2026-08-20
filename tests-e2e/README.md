# rcoder-e2e：Rust 黑盒 e2e 集成测试

对 `/computer/chat` + `/computer/progress/{session_id}`（SSE）核心链路的黑盒集成测试。
SSE 事件用 `shared_types` 类型消费——协议字段变更时测试**编译期报红**（契约守护）。

## 运行

```bash
# 配置：cp .env.local.example .env.local 后按需填写（LLM/K8s/入口 IP 全走配置，代码零硬编码）
cp .env.local.example .env.local

# compose 环境（本地 docker compose，前置 make dev-up + .env.local 的 LLM 配置）
make test-e2e-compose

# K8s 专项（个人测试机 20 / 229 单节点；19 机有生产环境禁用）
# 负载均衡场景默认 ignore（此前已验证通过），确认后 RUN_LB=1 显式开启
make test-e2e-k8s
make test-e2e-k8s RUN_LB=1        # 开启 lb 场景（等价 cargo test -- --ignored）

# 单场景过滤
cargo test -p rcoder-e2e --test compose_sse -- --test-threads=1 reconnect
```

环境门控（双层）：

1. 本 crate 不在 workspace `default-members`：裸 `cargo test` / `cargo build` 不碰它
2. `cargo test --workspace` 时每场景入口探测环境（compose /health 2s；K8s TEST_K8S_SSH + 首入口
   /health），不可达 → `verdict=skip`，无环境机器保持全绿（与 rcoder-storage PG-gated 同模式）；
   lb 三场景额外 `#[ignore]`——`cargo test --workspace` 不会跑，重跑须显式 `--ignored`

配置读取：环境变量 > 仓库根 `.env.local`（模板 `.env.local.example`）> 默认值。
关键项：`RCODER_URL`（默认 `http://127.0.0.1:8090`）、`LLM_API_KEY` / `LLM_BASE_URL` /
`LLM_MODEL`（chat 场景必需）、`LLM_MODEL_PRO`（切模型场景）、`LLM_BASE_URL_ANTHROPIC`
（acp-ts 后端）、`TEST_K8S_SSH` / `TEST_K8S_NS` / `LB_ENTRY_HOSTS` / `LB_NODEPORT`（K8s 专项；
单节点配一个入口 IP，入口轮换退化为同入口，SSE 语义断言仍有效）。

## JSONL 报告（供 agent 追溯排查）

每场景一个 `reports/<run_tag>_<pid>/<scenario>__<backend>.jsonl`，事件**实时逐行落盘**
（测试中途挂掉/被 kill，已收事件全部保留）。行类型：

| kind | 说明 |
|---|---|
| `scenario_begin` | 场景与环境信息 |
| `chat_request` | chat 留痕（请求脱敏——api_key 永不落盘；响应全量） |
| `subscribe_begin` / `subscribe_end` | SSE 订阅窗口与汇总（seqs/type_counts/拼接全文/结束原因） |
| `sse_event` | **连续的 SSE 消息流**（seq/event/data 原始 JSON/相对毫秒） |
| `assert` | `level=hard`（可穷举不变量，fail 即场景红）或 `level=diagnostic`（特征指标，不判死——缺失/重复的复杂形态由 agent 看报告判定，新异常模式沉淀为新 hard 断言） |
| `scenario_end` | verdict（pass/fail/skip/aborted）+ 断言计数 |

入口文件 `summary.json`（run 级）：全部场景 verdict + jsonl 路径 + 失败断言列表。

agent 排查示例：

```bash
# 先读 summary 定位失败场景
cat tests-e2e/reports/<run>/summary.json
# 看失败断言明细
grep '"level":"hard","ok":false' tests-e2e/reports/<run>/<scenario>.jsonl
# 逐条追 SSE 消息流（或按 seq 区间过滤）
grep '"kind":"sse_event"' tests-e2e/reports/<run>/<scenario>.jsonl | jq -c '{seq, event, t_ms}'
# 看订阅窗口汇总（拼接全文/seq 列表/结束原因）
grep '"kind":"subscribe_end"' tests-e2e/reports/<run>/<scenario>.jsonl | jq .
```

## 场景清单

**compose_sse**（语义与 Python 套件 tests/sse_e2e 逐一对齐）：

- `full_turn_openai / full_turn_anthropic`：chat 后立刻连 SSE——完整轮 + seq 单调
- `after_terminal_*`：turn 结束后连 SSE——0 消息事件（终端即清）
- `two_turn_isolation_*`：第二轮 seq 全 > 第一轮（轮次隔离）
- `reconnect_with_cursor`：断开带 Last-Event-ID 重连——只收增量
- `reconnect_no_cursor`：无游标重连——纯实时，零已收消息重放（红线）
- `model_switch`：同 session 切模型——零历史重放 + 上下文延续（需 LLM_MODEL_PRO）
- `concurrent_subscribers`：双客户端并发订阅——seq 交集 + 双端完整

**compose_userapp**：tasks/query 分页、publish 标识校验快速失败、publish 有限时间达终态
（activate 死锁修复行为面）、无 release lock 创建拦截。

**k8s_lb**（lb_test.py 完整移植）：入口轮换 4 轮、跨入口游标续传、新会话跨入口
（durable+回源 1s 验收窗口）。清理走 ssh kubectl（ns 硬限定 + user 前缀严格匹配）。

## 与 Python 套件的关系

双轨并存：Python（tests/sse_e2e）保留做快速迭代与临时排查；Rust 版定位 CI 门禁 +
协议契约守护。行为差异时以两者交叉验证为准。

## 布局

```
src/common/        # lib 目标（多测试目标共享编译一次）：env/gate/chat、SSE 客户端、
                   #   JSONL 报告器、场景编排件（collect_reported/spawn_chat）
tests/compose_sse.rs / compose_userapp.rs / k8s_lb.rs   # 三个测试目标
reports/           # .gitignore；JSONL 报告与 summary.json
```

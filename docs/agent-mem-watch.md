# Agent 命令树内存看门狗（Rust 进程内）

> 状态：**v2 待评审** | 分支：`feat/agent-mem-watch` | worktree：`../rcoder-memwatch`

## 1. 背景与目标

容器 `memory.max=8Gi` 时，一次超配（例：3 个并行 whisper ≈ 5.9GiB + 基线 2.5GiB）
会触发 **cgroup OOM，整容器被重建**，用户会话连同上下文一起消失。

**目标**：内存到达 `limit × 85%` 时，主动杀掉 **agent 命令树里最大的那个子进程**，
让容器活下来、任务以 `137/-15` 失败，并把原因留给上层。

**非目标**：

- 不做 swap / zram、不做 `/proc/meminfo` 虚拟化
- 不做 `oom_score_adj` / `memory.high`
- 不改 pod spec / securityContext（v1 只用容器内能力）
- 不用进程组 kill（claude 是 `setpgid(0,0)` 组长，会把会话一起杀掉）

## 2. 为什么放在 agent_runner 进程内

`pid` 来自 spawn 的返回值（`crates/agent_abstraction/src/launcher/lifecycle/spawn.rs`）：

```rust
let pid = child_process.id().filter(|pid| *pid != 0)…;
```

进程内实现的好处：拿到**权威 pid**、天然支持多会话、生命周期绑定、零额外进程。

## 3. 模块与落点

| 文件 | 改动 |
|---|---|
| `crates/process_utils/src/mem_watch/mod.rs` | `Config` / `Registry` / 采样循环 `start()` |
| `crates/process_utils/src/mem_watch/proc_tree.rs` | `/proc` 扫描、后代集合、候选选择（**纯函数**） |
| `crates/process_utils/src/mem_watch/cgroup.rs` | 读 `memory.current` / `memory.stat`，算 working set |
| `crates/process_utils/src/mem_watch/kill.rs` | pidfd 终止 + 冷却 |
| `crates/process_utils/src/lib.rs` | `pub mod mem_watch;` |
| `.../launcher/lifecycle/spawn.rs` | spawn 后 `register(project_id, pid)`；reaper 返回后 `unregister` |
| `.../launcher/claude_code_sacp/connection/message_loop.rs` | `process_prompt()` 首次调用 → `mark_session_active(project_id)` |
| `crates/agent_runner/src/main.rs` | `agent_runner_run()` 启动单例 task |
| `crates/process_utils/Cargo.toml` | `dashmap`（workspace 已有） |

**Registry 按 `project_id` 索引**（spawn 时必有，prompt 时也能拿到），
不是按 pid——否则 `process_prompt` 里没有 pid 可用，钩子挂不上。

## 4. 数据流

```
spawn agent ──► pid ──► register(project_id, pid, exe, starttime)
                          │
        每 500ms（单例 task）▼
   working_set = memory.current - memory.stat(inactive_file)
   working_set ≥ limit×0.85 ?  ── no ──► 睡到下一 tick
        │ yes
        ▼
   对每个 root：descendants(root) 过滤（见 §5）→ 候选
        │
        ▼
   victim = 全局 RSS 最大；本 tick 只杀 1 个
        │
        ▼
   tokio::spawn(kill_sequence)   ← 不阻塞采样循环
        │
        ▼
   pidfd_open → SIGTERM → grace → SIGKILL → 冷却 10s
        │
        ▼
   tracing::warn!(event="mem_watch.evict", …)
```

## 5. 候选选择规则（agent 无关）

### 5.1 永远生效的结构保护

候选 = `descendants(root)` 减去：

1. `root` 自己；
2. registry 里**其它会话的 root**（防串杀）；
3. PID 1；
4. RSS < `max(64MiB, limit × min_ratio)`。

### 5.2 自动识别 agent 本体：窗口由**第一条用户消息**关闭

关键事实：launcher 启动的是 `claude-code-acp-ts`（shebang 脚本），内核 exec 后
`exe=/usr/local/bin/node`；真正的 agent 二进制是 wrapper 内部另起的进程
（`exe=.../claude-code-acp-ts/.../claude`）。**按 node / 包名判断都只能覆盖一半。**

所以用**事件**而不是名字和时间：

```
打开：root spawn
内容：每 200ms 快照 root 的后代 → 加入「会话进程」永久保护
关闭：收到第一条用户消息（process_prompt 首次调用），关闭前做一次【同步最终快照】
兜底：bootstrap_cap_ms（默认 300_000 = 5 分钟）到点强制关闭，同样先做最终快照
```

**为什么边界是精确的**：用户消息到达前，agent 不可能执行用户命令。
所以"首条消息前出现的所有进程" ≡ "agent 本体 + 初始化子进程"，与启动快慢无关。

**为什么必须做最终快照**：首条消息可能落在两次 200ms 采样之间，
此时 `claude` 刚起来还没被采样；不做同步快照，它会在窗口关闭后立刻变成候选。

**为什么要有 cap**：harness 等不走 `process_prompt` 的运行时，窗口永远不关 → 永远不杀。
cap 到点强制关闭可以让功能在"没有钩子"时也能工作（此时按静态规则 4/5 兜底）。
cap 关闭时记录 `event="mem_watch.bootstrap_timeout"`，便于发现漏挂钩子。

### 5.3 钩子位置

`crates/agent_abstraction/src/launcher/claude_code_sacp/connection/message_loop.rs`
的 `process_prompt()` 首次调用处：

```rust
mem_watch::mark_session_active(&project_id);   // 幂等；内部做最终快照
```

### 5.4 静态兜底规则（与窗口叠加）

| # | 规则 | 覆盖 |
|---|---|---|
| 1 | `pid == root` | root 就是 agent（harness） |
| 2 | 其它会话的 root | 多会话防串杀 |
| 3 | 窗口内快照到的后代 | agent 本体及初始化进程（**主力**） |
| 4 | `exe == root.exe` | node 系 MCP 子进程 |
| 5 | `exe` 路径包含 `basename(root.cmdline[1])` | 包名出现在 exe 路径里的 SDK 二进制 |

用 `/proc/<pid>/exe`，不用 `comm`（内核截断到 15 字节）。

### 5.5 实测验证（生产）

```
root   pid=3407241 exe=/usr/local/bin/node  cmdline="node /usr/local/bin/claude-code-acp-ts"
  ├─   pid=3407258 exe=.../claude-code-acp-ts/.../claude    ← 规则 3 / 5
  └─   pid=3407268 exe=/usr/local/bin/node                  ← 规则 4
```

### 5.6 残留风险

| 场景 | 结果 | 影响 |
|---|---|---|
| 首条消息之后才起的 MCP 服务 | 可能被杀 | 会话不死，工具报错可重连 |
| 首条消息之后才起的 agent 子进程 | 可能被杀 | 静态规则 4/5 兜底 |
| 无钩子且 5 分钟后才启动 agent | cap 关闭后可能误杀 | 调大 cap 或补钩子 |
| 用户首条消息极早（agent 还在初始化） | 该阶段进程未受保护 | 概率低；agent 初始化通常先于 UI 可用 |

## 6. 触发指标：**working set，不是 memory.current**

`memory.current` 包含 page cache。一个命令 `cat` 2GB 文件会让 current 冲到 2GB，
但内核可以直接丢弃缓存——此时杀进程是**误杀**。

```
working_set = memory.current - memory.stat.inactive_file
```

与 Kubernetes 驱逐用的 `container_memory_working_set_bytes` 语义一致，
也是"内核真正回收不动"的部分。**v1 必须用这个口径。**

## 7. 控制开关（热更新，不重启容器）

四层来源 + 内置默认，优先级从高到低，每 1s 比对内容指纹：

| 优先级 | 路径 | 作用域 | 生效 | 改动成本 |
|---|---|---|---|---|
| 1 | `/tmp/mem-watch.conf` | 单容器 | <1s | 0（exec） |
| 2 | `/home/user/.rcoder/mem-watch.conf` | 单用户所有容器 | 秒级 | 0（PVC 已挂载） |
| 3 | `/etc/agent-runtime/mem-watch.conf` | 全集群 | ~1min | rcoder 加 optional volume |
| 4 | `RCODER_MEM_WATCH*` env | 容器启动默认 | 重建容器 | pod spec |
| 5 | 代码内置默认 | — | — | — |

```ini
mode             = on        # off | dry | on
ratio            = 0.85
interval_ms      = 500
min_ratio        = 0.08
grace_ms         = 3000
cooldown_ms      = 10000
bootstrap_cap_ms = 300000
# 仅救火用，默认没有
# protect        = /opt/some-agent/bin/
```

解析规则：

1. 文件不存在 → 下一层；
2. **解析失败保留上一份有效配置，只打一次 error**；
3. 未知键忽略；
4. `mode=off` 立即停止新的 kill（进行中的终止序列走完）。

运维操作：

```bash
kubectl -n nuwax-k8s-test exec <pod> -c agent -- sh -c 'echo "mode=dry" > /tmp/mem-watch.conf'
echo "mode=on" > /home/user/.rcoder/mem-watch.conf
kubectl -n nuwax-k8s-test patch configmap agent-runtime-flags --type merge \
  -p '{"data":{"mem-watch.conf":"mode=on\n"}}'
```

坑：ConfigMap **必须目录挂载**（subPath 不更新）；kubelet 同步约 1 分钟；
第 3 层是可选增强，v1 只用 1/2/4/5。

## 8. 终止与失败处理

- `pidfd_open` + `pidfd_send_signal`（nix 0.31 `process`/`signal` feature）；
  版本不支持则退回 `libc::syscall`。
- 终止序列在独立 task 里跑，**不阻塞采样**。
- `ESRCH` → 忽略；`EPERM` → 记一次 error 并自动降级 `off`（AppArmor 场景）。
- 每次 kill 后 `cooldown_ms` 内不再杀同一进程族。
- **每 tick 最多杀 1 个**，避免连锁误杀。
- registry 每 tick 校验 `starttime`，root 消失或 pid 被复用则清理。

## 9. 可观测

| 事件 | 说明 |
|---|---|
| `mem_watch.started` | 启动参数快照 |
| `mem_watch.bootstrap_sealed` | 窗口关闭（reason=first_prompt / cap） |
| `mem_watch.would_evict` | dry-run 候选 |
| `mem_watch.evict` | 实际执行（pid/rss/comm/project/working_set） |
| `mem_watch.no_candidate` | **超阈值但无可杀对象**（全被保护或都太小）——没有这条就无法判断"看门狗是否在工作" |
| `mem_watch.disabled` | EPERM / 无限制 / 关闭原因 |

Phase 2（可选）：`/computer/status` 只读暴露 `mode / evicted_total`。

## 10. 测试

| 层级 | 内容 |
|---|---|
| 单测 | `/proc/<pid>/stat` 解析（comm 含空格/括号）、后代遍历、候选过滤 |
| 单测 | working set 计算（构造 memory.stat 文本） |
| 单测 | **窗口竞态**：先 mark 后 spawn 子进程 → 子进程仍被保护（验证最终快照） |
| 单测 | 选择器：只选最大、不选 root/其它 root、`min_ratio` 边界 |
| 单测 | pid 复用：starttime 变化 → 条目被清理 |
| 集成 | 假树（父 + 3 个 hog）：dry 不杀；on 杀 1 个且父存活 |
| 集群 | 复用已验证的 3 组对照，Rust 版复跑确认 `restarts=0` |

## 11. 灰度与回滚

1. 默认 `off`（内置），合并零影响；
2. dev 节点设 `dry`，观察一周 `would_evict` / `no_candidate` / `bootstrap_sealed`；
3. 确认候选都是 python/ffmpeg/编译进程后，单节点 `on`；
4. 回滚：`mode=off` 或 env 改回，**不需要发版**。

## 12. 提交拆分

1. `feat(process_utils): /proc tree + working set + victim selection (pure, tested)`
2. `feat(process_utils): mem_watch registry + sampler + kill (dry-run)`
3. `feat(agent_runner): register roots, seal on first prompt, start sampler`
4. `docs: rollout`

## 13. 风险与取舍

| 风险 | 缓解 |
|---|---|
| page cache 触发误杀 | 用 working set 而非 current（§6） |
| 首条消息竞态 | 关闭窗口前同步最终快照（§5.2） |
| 多会话杀错会话 | v1 全局选最大；v2 可按会话增长量选 |
| 采样漏掉极快尖峰 | 500ms + 85%；已知限制，dry 期统计 |
| task panic 静默失效 | JoinHandle 监督 + 重启 + error 日志 |
| PID 复用 | pidfd + starttime 校验 |
| 钩子漏挂导致永不杀 | `bootstrap_cap_ms` 兜底 + `bootstrap_timeout` 日志 |

## 14. 待确认

1. `mode` 内置默认 `off` 是否合适（推荐）。
2. `ratio=0.85` / `interval=500ms` / `min_ratio=0.08` 是否需要按 8Gi 场景微调。
3. harness 是否也经过 `process_prompt`；若否，先靠 cap 兜底还是同步补钩子。
4. Phase 2 的 HTTP 指标是否现在就要。

## 15. 实施进度

| 模块 | 状态 |
|---|---|
| `proc_tree.rs`（/proc 解析、后代、候选选择） | ✅ 完成 + 8 单测 |
| `cgroup.rs`（working set） | ✅ 完成 + 5 单测 |
| `config.rs`（参数与模式） | ✅ 完成 + 4 单测 |
| `switch.rs`（文件开关、热更新、坏文件保留旧值） | ✅ 完成 + 5 单测 |
| `registry.rs`（会话注册、启动窗口快照、封窗、剪枝） | ✅ 完成 + 5 单测 |
| `kill.rs`（rustix pidfd，禁 unsafe） | ✅ 完成 + 2 集成测试（Linux） |
| `sampler.rs`（采样循环、bootstrap cap、冷却） | ✅ 完成，待接 registry |
| `examples/mem_hog.rs` + `examples/mem_watch_e2e.rs` | ✅ 端到端验证用 |
| 接入 `spawn.rs`（注册/注销） | ✅ 完成（各 1 行） |
| 接入 `message_loop.rs`（首条 prompt 封窗） | ✅ 完成（幂等，已封窗不再扫 /proc） |
| 接入 `main.rs`（`install_and_start` + panic 监督） | ✅ 完成 |

约束：workspace 禁止 `unsafe`（`rust.unsafe_code = "deny"`），因此 pidfd 用
`rustix::process::pidfd_open/pidfd_send_signal` 的安全封装，而不是 `libc::syscall`。

### 15.1 实测结果（本地 Docker / Linux）

```
cargo test -p process_utils --lib mem_watch     → 31 passed（含 2 个真实 pidfd 终止）
examples/mem_watch_e2e（--memory=512m）          → alive=1 dead=2, root 存活, 无容器 OOM
```

e2e 输出：

```
root pid=369 exe="/t/debug/examples/mem_watch_e2e"
sealed=true
spawned hog 0 pid=388
spawned hog 1 pid=389
spawned hog 2 pid=390
hog 150MB ready pid=390
hog 150MB ready pid=389          ← pid 388 没来得及打印就被淘汰
RESULT alive=1 dead=2
OK
```

说明：阈值 40% 时杀到"低于阈值"为止（先杀 388，仍高于阈值，冷却后再杀 389），
**agent 根进程全程存活**，容器的 `memory.max` 没有被撞到。

## 16. 代码评审记录（v2 修复项）

| # | 问题 | 后果 | 修复 |
|---|---|---|---|
| 1 | 注册表按 `project_id` 做 key | 同 project 的并发会话互相覆盖，先注册的会话失去保护 → 可能被自己人杀掉 | 改为按 pid 索引、按 project 分组；封窗作用于该 project 的所有根 |
| 2 | 保护集只记 pid | 受保护进程退出后 pid 被复用，新命令"继承"永久保护 → 永远杀不掉 | 记 `(pid, starttime)`，比较时才认 |
| 3 | `seal` 只封第一个匹配的 root | 多会话时其余会话窗口不关，功能半失效 | `seal_project` 遍历所有未封窗 root |
| 4 | `root.exe` 读取失败时为空路径 | 空 == 空 ⇒ 所有读不到 exe 的进程都被保护 | 空路径不参与相等判断 |
| 5 | `min_rss_floor` MB→字节用乘法 | 环境变量给极大值时溢出 panic | `saturating_mul` |
| 6 | 开关文件读失败 = 文件不存在 | 一次瞬时读失败会把 `on` 退回默认（大概率 off） | 区分「不存在」与「存在但读不出」，后者保留上一份有效配置 |
| 7 | `interval_ms` 覆盖不生效 | 改开关文件里的采样周期没反应 | 采样循环改用 `sleep`，每轮重新取配置 |
| 8 | 每 tick 都扫 `/proc` | 2Hz × ~150 进程的无谓读取 | 全部封窗且未超阈值时跳过 |
| 9 | dry-run 每 tick 记一条 | 一周日志被 `would_evict` 淹没 | 与真实淘汰共用 cooldown 节流 |
| 10 | `script_hint` 可能取到参数 | `node --inspect` 时 hint 无意义 | 跳过以 `-` 开头的 `cmdline[1]` |

### 16.1 已知行为（不是 bug，但要知道）

- **运行期从 `off` 切到 `on`**：第一次活跃 tick 会把当下所有进程当作"启动窗口"保护起来
  （因为无法回溯区分"agent"与"命令"）。结果是：切换前就在跑的重任务不会被杀，
  只有切换后新起的命令才会被淘汰。这是安全方向，但会让"开了没效果"看起来像 bug。
- **`node` 系进程全被保护**（规则 4：`exe == root.exe`）：用户自己跑的 node 脚本也不会被淘汰。
- **多会话时按全局最大 RSS 选**：可能杀掉"没在泄漏的那个会话"的命令（v1 限制）。

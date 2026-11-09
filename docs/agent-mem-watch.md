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

| 11 | 只杀 victim 单进程 | 命令树常是 `bash -c "..."`：杀掉 bash 后 python 被 reparent 到 PID 1，**脱离 `descendants(root)` 范围**，内存不释放且再也不会被选中 | 改为杀 victim 的整棵子树（排除 PID 1/root/受保护进程），新增 `subtree_targets` 纯函数 + 单测 |
| 12 | 快速路径不校验 root 存活 | root 死后 pid 被复用，`descendants(新进程)` 会把无关进程当命令 | 快速路径加 `prune_dead_light`（每 root 一次 stat） |

### 16.1 已知行为（不是 bug，但要知道）

- **运行期从 `off` 切到 `on`**：第一次活跃 tick 会把当下所有进程当作"启动窗口"保护起来
  （因为无法回溯区分"agent"与"命令"）。结果是：切换前就在跑的重任务不会被杀，
  只有切换后新起的命令才会被淘汰。这是安全方向，但会让"开了没效果"看起来像 bug。
- **`node` 系进程全被保护**（规则 4：`exe == root.exe`）：用户自己跑的 node 脚本也不会被淘汰。
- **多会话时按全局最大 RSS 选**：可能杀掉"没在泄漏的那个会话"的命令（v1 限制）。
- **子树终止仍有极小竞态**：从读进程表到发信号之间新 fork 的子进程不在终止列表里，
  父进程死后它会 reparent 到 PID 1 并脱离范围。窗口是毫秒级，v2 可用 SIGSTOP 后再扫描消除。

## 17. 记录的生命周期（agent 被回收后会发生什么）

| 场景 | 注册表记录 | 由谁清理 |
|---|---|---|
| agent 正常退出 / 被信号杀死 / 被 cancel 后 kill | 删除 | reaper：`wait()` 返回后 `try_unregister(project, pid)`（无条件执行） |
| reaper 来不及执行（任务被 abort / 运行时关闭） | 残留 → 下一个 tick 删除 | sampler 剪枝 |
| pid 被复用（root 已死、pid 给了别人） | 删除 | 剪枝比较 `starttime`，不一致即删 |
| 整个 agent_runner 进程退出/容器重建 | **全部消失** | 注册表是进程内内存，无持久化 |

采样有两档剪枝：

- **快速路径**（全部封窗 + 内存未超阈值）：`prune_dead_light` 只对每个 root 读一次
  `/proc/<pid>/stat`（N ≤ 并发会话数），校验 `starttime`；
- **慢路径**（有未封窗会话，或内存超阈值）：已有完整进程表，`prune_dead` 全表校验。

> 为什么快速路径也要剪：否则 root 死后 pid 被复用，`descendants(root_pid)` 会把
> **无关进程的子进程**当成该会话的命令，从而误杀。

残留的唯一窗口：`mode=off` 期间 sampler 不跑（`step` 不执行），此时只有 reaper 在清理；
但该状态下看门狗本来就不动作，不存在误杀风险；重新打开后的第一个 tick 会先剪枝。

## 18. 公共化与收敛（与 agent_resource_monitor 的关系）

### 18.1 `/proc` 扫描：已收敛为一份

新增 `process_utils::proc_scan::snapshot()`（`pid -> ProcInfo`），`ProcInfo` 现在同时提供
`ppid / pgrp / comm / rss_bytes / starttime / utime_ticks / stime_ticks`，
字段下标、`comm` 含括号、rss 页→字节换算只维护一处。

| 消费者 | 用途 | 迁移前 | 迁移后 |
|---|---|---|---|
| `mem_watch` | 建进程树、选淘汰目标、校验 pid 复用 | 自己解析 | `proc_scan::snapshot()` |
| `agent_resource_monitor` | 按 pgid 聚合 RSS / CPU ticks | `procfs::all_processes()` 逐进程 `stat()` | `proc_scan::snapshot()` |

资源采集只保留了 `procfs::ticks_per_second()`（时钟频率与扫描无关），
聚合语义（按 `pgrp` 过滤、RSS 相加、utime+stime 相加、空集合返回 `None`）逐条对齐。

### 18.2 会话注册表：**不合并**，但要幂等

`AGENT_REGISTRY` 在 `agent_runner::service`（二进制 crate），`mem_watch` 在
`process_utils`（被 `agent_abstraction` 依赖的库）——让库依赖二进制的注册表会**反向依赖**，
所以不能直接复用。两者职责也不同：

| | `AGENT_REGISTRY` | `mem_watch::registry` |
|---|---|---|
| 层级 | RPC / 会话视图 | 进程树视图 |
| 键 | project_id / session_id | pid |
| 存 | 会话身份、lifecycle handle、pgid | starttime、启动窗口快照、受保护集合 |
| 生命周期 | 业务会话 | 进程存续 |

真正需要保证的是**重复登记不能破坏窗口状态**，因此 `Registry::insert` 改为幂等：
同一 `pid` 且 `starttime` 相同则复用既有 `Root`（不重置 `spawned_at`、不清空保护集）；
`starttime` 不同（pid 复用）才替换。这样将来若有第二条登记路径（例如从
`AGENT_REGISTRY` 桥接），也不会把已封窗的会话重新打开。

### 15.2 场景化验证（`examples/mem_watch_scenarios.rs`，`--memory=512m`）

| 场景 | 配置 | 断言 | 结果 |
|---|---|---|---|
| 1 off | 文件 `mode=off`，hog 250MB（单进程即超 40% 阈值） | hog 存活 | ✅ |
| 2 on + 窗口 | 删开关文件（回落默认 On）→ 封窗 → 新 hog 150MB | **封窗前**的 hog 存活（窗口保护），封窗后的新 hog 被淘汰 | ✅ |
| 3 dry | 文件 `mode=dry`，hog 150MB | hog 存活（只记录） | ✅ |

全程根进程存活、容器未被 OOM。场景 1 特意让 hog **单个即超阈值**——否则"off 不杀"
可能只是"内存没到阈值"，不算真验证；写完开关后也各等 1.5s 让配置生效，
排除「残留 on 窗口内被杀」的假阴性/假阳性。

首次运行发现的问题：`mem_watch::start` 需要 tokio 上下文，example 里改为
自建 runtime `rt.spawn(run(cfg))`（同时导出 `mem_watch::run`）。

## 19. 单节点 k8s 部署验证（192.168.31.45，k3s v1.35.4）

部署脚本：`scripts/deploy-oom-test-45.sh`（45 的 docker 内容存储损坏——save 只出
37KB 空壳 tar，同 AGENTS.md 已知问题 2 的形态——所以完全绕开 docker：
`k3s ctr images tag` 复用已有生产镜像（零拷贝）+ hostPath 注入新二进制 +
initContainer 覆盖 `/usr/local/bin/agent_runner`）。

### 19.1 结果

| Pod | 内容 | 结果 |
|---|---|---|
| wd-integration | **真实 agent_runner 二进制**（完整沙箱镜像），`RUST_LOG=info` | ✅ `mem_watch.started(mode=off)` 落 telemetry 文件日志；`kubectl exec` 写 `/tmp/mem-watch.conf` → `mem_watch.mode_changed(off→dry)` |
| wd-behavior | 场景化 e2e（`--memory=512m`，k8s 托管 cgroup） | ✅ 三场景 PASS（同 §15.2） |

### 19.2 两个**必须在生产部署前确认**的发现

**① AppArmor 拒绝容器内 signal → 看门狗静默失效（本次实测踩中）**

该节点 k8s（Ubuntu 24.04 + containerd）给容器挂 `cri-containerd.apparmor.d`（enforce），
profile 拒绝向 peer 发信号 ⇒ `pidfd_send_signal`/`kill` 全部 `EPERM`，
evict 事件每 500ms 刷一条但进程死不掉。加
`securityContext.appArmorProfile: {type: Unconfined}` 后三场景立即全 PASS。

⇒ **rcoder 给沙箱 pod 的模板需要二选一**：
- `appArmorProfile: {type: Unconfined}`（简单，代价是放弃该层 LSM）；
- 或节点下发 Localhost profile（默认规则 + `signal (send) peer=<自身>`）。
- 且**必须在启动时自检**（代码已实现：EPERM → `mem_watch.disabled` 日志并自动停用），
  另外灰度前先在目标节点 `dmesg | grep 'operation="signal"'` 验证。

**② 该集群容器内 `/sys/fs/cgroup/memory.max` 是 `max`（host cgroup namespace）**

看门狗读不到限额 → 保持空闲。已支持 `RCODER_MEM_WATCH_LIMIT_BYTES` env 覆盖
（rcoder 创建 pod 时知道限额，注入即可）。生产集群部署前需确认：
```bash
kubectl exec <agent-pod> -- cat /sys/fs/cgroup/memory.max   # 8589934592=私有ns；max=host ns
```
是 `max` 就必须注入 env（或改集群 kubelet `cgroupNamespaceMode: private`）。

### 19.3 与生产集群（192.168.1.19…）的差异

| 检查项 | 45（测试） | 生产（待确认） |
|---|---|---|
| AppArmor signal | ❌ 拒绝 | 未验证（agent_runner 从不杀进程，无 DENIED 记录不能证明放行） |
| 容器内 memory.max | max（host ns） | 未验证 |
| 验证方法 | — | `kubectl exec` 一条 python kill 自身子进程 + `cat memory.max` |

### 19.4 部署形态的临时性

hostPath 注入 + initContainer 覆盖是**测试专用**形态。生产正确做法：
`dev-oom` 分支构建正式镜像（`build-agent-runner-amd64` 等），rcoder 的 pod 模板加
appArmorProfile 与 `RCODER_MEM_WATCH_LIMIT_BYTES` env。

# 修复后第二轮复核

日期：2026-09-16。基线：`baf8d83d`，含 `154e126e`、`85c69022`。开始时工作树干净。

## 结论

不能关闭全部 R01–R09，也不能称原始四项任务全部开发完成。R05 的事件判定、R07 的占位参数退役、R08 的限定旧 JSON 兼容，源码修改方向正确，本轮未发现同类阻断缺陷；其运行验收仍需独立证据。R01–R04、R06、R09 仍有以下明确问题。

本轮为源码和提交差异复核，未修改业务代码，未执行 Cargo/Compose/K8s，也未独立复现对方报告的全量测试与两项基线失败。下列触发链是源码推导，不冒充运行结果。

## 尚未解决及新增问题

### B01 / P1：Idle 下 Stop 永远没有终态（R01）

- `crates/app-cli/src/server.rs:1086`：Stop 入队不再设置 current_runtime_operation。
- `server.rs:1396`：Idle 消费控制信号仍用 `{ .. }` 丢掉 operation_id。
- `server.rs:1431`：停止完成仍调用 finish_current_runtime_operation。

**反例**：空闲服务 current=None → Stop B Accepted → Idle 取出 Stop 但丢 B → 停止完成尝试 finish(None)，B 永远 Accepted。内核 active 仍为 B，后续 Start 返回 busy。正常停止后再 Stop 也可能触发。

**修复要求**：InitialAction/执行上下文始终携带 ID；所有成功和失败分支按该 ID 收束，覆盖 Idle/Running/启动中三类状态，而非只补 Running select。

### B02 / P1：builtin 取消后重复轮询完成的 JoinHandle（R03，新引入）

- `server.rs:1664`：取消分支执行 join_supervisor，将 sup 完成并标记 sup_joined=true。
- 该分支未 continue/return，随后进入 `server.rs:1693` 的 `outcome = &mut sup`。
- `server.rs:1000` 的 joined 防重逻辑仅保护 helper，直接 select 不受保护。Tokio task/core 明确在再次读取完成结果时 panic：`JoinHandle polled after completion`。

**反例**：builtin 启动期间收到取消 → running 通知到达 → 停进程且 join 成功 → 再 poll 同一 JoinHandle → 驱动任务可能 panic，不能可靠继续提供控制。

**修复要求**：取消清理成功后直接转换到 Idle 下一轮；保证 JoinHandle 唯一收割，不仅在 helper 中防重。增加真实 server_loop 取消路径测试。

### B03 / P1：Stop 未形成旧启动提交屏障，取消检查与提交也不原子（R01/R03）

- `runtime_kernel.rs:595` 附近 Stop 写 Stopped/推进 revision，但只把 Stop 入队。
- server 的启动和 complete_running 路径没有复核此操作的 expected_revision；启动中也不消费 Stop。A 仍可完成启动并宣告 Succeeded，随后才执行 B。
- `server.rs:1555`、`:1664` 的取消检查位于 complete_running 之后；检查与 finish(Succeeded) 之间没有与 request_cancel 共享的提交同步边界。
- `runtime_kernel.rs:708` 的“墓碑”是内存 HashSet，不是落盘取消记录；不能将其描述为持久取消意图。

**反例**：A 慢启动 → B Stop 已受理且 revision 已变 → A 仍提交 Running/Succeeded。另一个反例是取消检查返回 false 后、finish 前受理取消，最终仍提交成功。

**修复要求**：定义取消/停止受理与启动完成的线性化边界，提交前在同一状态机约束下检查身份、revision、停止和取消；清理未知不得释放保护。检查点取消可以是设计选择，但必须满足已有 Spec 的旧提交屏障，不能仅将“下一边界才生效”视为豁免。

### B04 / P1：状态根仍由 parent() 推导，source/.run 锁域未统一（R02）

- `runtime_kernel.rs:77`：root=workspace.parent()/.app-cli-state。
- `server_journal.rs:70`：锁根同样由 workspace.parent() 推导。

**反例**：workspace=/data/app 与 workspace=/data/app/.run 分别得到 /data/.app-cli-state、/data/app/.app-cli-state，锁也在不同目录。热部署换代稳定这一子问题有改善，但“同一应用运行态单一所有者”没有实现。前者甚至会把后者的状态目录识别为自己的 legacy 位置。

**修复要求**：显式、稳定、按应用隔离的 state_root；source/.run/别名必须归同一身份与锁域。测试必须包括这两个入口竞争，而非只断言父目录不在 workspace 内。

### B05 / P1：恢复保护只约束新 API，自动启动和旧部署仍可绕过（R04/R03）

- `server.rs:839` 先 initialize_startup，再 recover 新内核；`:866` 遇 recovered 只记日志，没有撤销 first_request。
- `server.rs:874` 用 matches!(load_desired(), Ok(Stopped))；损坏 desired 的 Err 被等价处理为“不是 Stopped”，仍可启动。
- `server.rs:889` 内核装配失败只关闭新 API，仍保留旧部署与自动启动。
- `server.rs:503` 的旧 try_accept_deploy_with_id 不检查 RuntimeKernel 恢复保护，仍是另一套受理。

**反例**：release.lock 存在、desired=Running、operations 有损坏记录 → recover 阻止新 runtime 操作，但 first_request=Existing 仍启动业务。或者双状态域导致 kernel open 失败，旧链继续运行。新内核的 fail-closed 不能代表整个运行态 fail-closed。

**修复要求**：可信状态读取/恢复裁决先于启动决策；恢复保护作用于所有写入口。初始化失败不得以 legacy 路径继续修改同一运行态。明确有意的新部署与 Pod 重建时沿用的 env，不能把后者天然视为新授权。

### B06 / P1：workspaceType 仍未遵守唯一定位契约（R06）

- `crates/file-server/src/extract.rs:186` 合并链仍包含 body serviceType 与旧 x-service-type。
- Git 也调用这一 helper，所以“Git serviceType 不参与目录选择”的交付描述不成立。
- 当前 TS `src/utils/computer/workspaceContext.js:17` 明确：工作目录只用 workspaceType；serviceType 是运行时路由，缺省 taskAgent。

**反例**：没有 workspaceType，但 x-service-type=userapp 且 appId 存在：Rust 选择 UserApp，TS 选择 taskAgent。传未知 workspaceType 时也可能被旧字段转向其他目录。

**修复要求**：删除工作目录选择中的两级旧字段回退。保留字段用于独立路由不等于允许它参与定位。“滚动升级”并不是本需求授权的兼容行为。测试最终路径及新旧字段并存，不能用断言旧回退正确来代替契约测试。

### B07 / P1：退避期间仍 poll watcher，外层退避无效；429 测试被底层重试掩盖（R09）

- `crates/docker_manager/src/runtime/k8s_observation.rs:232` 的 pending_backoff 分支同时 select sleep(wait) 与 stream.next()。
- watcher 在 next poll 就开始恢复；这里正好立即 poll。快速错误到达时先于 sleep 返回，循环继续，未产生所声明的 200ms→2s 请求间隔。
- 新测试 `k8s_observation_tests.rs:264` 使用 `Config::new` 和默认 kube client。锁定版本 kube-client Config.default_retry 默认 true；client/builder.rs 安装 RetryLayer，client/retry.rs 对 HTTP 429/503/504 本来就退避。因此 <=10 次的测试不能区分外层退避有没有工作。
- kube-client retry.rs 实际还支持 HTTP Retry-After；verification 的“Status 不透出所以无法遵循”混淆了 HTTP 客户端与 watch 事件两个层次。

**修复要求**：等待退避时只等待 sleep/cancel/deadline，完成后再 poll watcher，或者使用正确的 backoff stream 包装。测试外层时禁用客户端 default_retry，覆盖快速 HTTP 500、流内暂态 ERROR、429；断言间隔/请求数及取消。正常客户端组合再单独验收，不关闭生产重试来规避问题。

### B08 / P1：source profile 仍未真正适配两种执行引擎（R03，未完成范围）

- `runtime_kernel.rs:449` 接受 Start/Restart+Source，身份仍声明 source-profile。
- 派发 OrchestrateSource 仅携带 operation_id，最后变成 Existing，未建立共享 ResolvedRunPlan。
- `crates/app-cli/src/supervisord_host.rs:176` 仍用 spec.run.command，不能据此满足 Source 要求的 devrun 优先。

**修复要求**：按原 P2-05 接入 source devrun/devbuild/static 语义与两引擎一致的运行计划，或者在实现前明确拒绝/不宣告该能力。ArtifactId 被显式拒绝属于合理止血，但需求本身仍未开发。

## 测试证据为什么不足

1. `runtime_kernel.rs:1047` 的 executing_identity_refuses_concurrent_overwrite 是空测试，没有执行与断言。
2. finish_by_id_does_not_complete_a_different_operation 直接调用 kernel.finish(A)，没有调用真实 dispatch/Idle/Running 主循环，所以不能发现 B01。
3. 取消测试只检查 HashSet 插入/清除，没有执行 join/select 流程，覆盖不到 B02。
4. 429 用例受默认 client 重试影响，覆盖不到 B07 的外层失败。
5. 根 Cargo.toml 排除了 crates/app-cli；workspace 全量测试数不能作为 app-cli 已被全量覆盖的证据。对方确实另报 app-cli 测试，但上述空白仍在。

这些问题不代表所有已报测试结果不真实，而是通过的测试没有证明所宣称的行为。应先补能在当前代码上失败的执行路径反例，再修代码。

## 原始任务是否全部开发

| 工作项 | 当前事实 | 是否完成 |
|---|---|---|
| 启动错误传播阶段一 | bind/错误退出/Child 监督/Done 竞争等已有实现；预算 verification 仍承认合法慢启动可能被截断；实机验收未闭环 | 部分实现，不能整体验收 |
| 单一运行态所有者阶段二 | API/内核有代码，但 B01–B05/B08 未解决；稳定显式状态根、统一 worker/旧 API、共享运行计划、ArtifactId、attach 未完整落地 | 未完成 |
| 平台迁移阶段三 | file-server 仍有 start_dev_manifest 的 legacy spawn 路径；未完成 managed 协议及镜像迁移 | 未开发完成 |
| UserApp 去用户绑定 | 主链已有代码；R07/R08 此次有针对修复，尚需无 header、多占位、升级数据与普通 Computer 回归 | 主要代码已落地，验收未完成 |
| file-server 同步 | workspaceType 仍有 B06；service/skills.rs、agent_store.rs 仍明确 manifest 并集视图“暂缓/过渡防线” | 未完成 |
| kube-runtime A | Pod watch 有实现，外层退避仍有 B07；实机及性能证据缺失 | 未完成验收 |
| kube-runtime B | Builder STS/Pod 双资源观察未迁移 | 未开发 |
| kube-runtime C | Events publisher/RBAC 未接入 | 未开发 |
| kube-runtime D | 删除观察 | 原 Spec 明确为可选后续 |
| 发布 | npm/镜像发布未执行 | 未交付发布 |

**范围修正**：原 kube-runtime spec.md 将 Events 列在目标中，tasks.md T06 也要求实现；只有删除观察被明确标成可选。因此不能依据后写 verification 把 C 自动降级成可选。上一轮审查文档沿用了该说法，本轮以原 Spec 校正：B/C 属于原任务未完成，D 可选。

现有 Tasks 仍保留“仅规划/所有待执行”的旧状态，不能用勾选数计算完成率；应按上表与当前源码更新，并补真实验收证据。

## 下一轮修复交接

先完成 B01–B08 的失败反例和修复，特别是控制 worker 的真实状态转换，避免继续只修 helper。为保持与正在运行的集成测试隔离，先确认被测源码冻结状态，必要时在独立 worktree 与独立 target 开发。不要修改旧测试报告或把本轮新缺陷划为“以后可选”。

完成后分别交付：①本轮逻辑缺陷修复；②原需求剩余开发；③Compose/K8s 验证；④发布。每一项明确源码提交、命令/退出码、证据和未运行项，不能用①代替②–④。

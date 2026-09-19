# 2026-09-19 独立复核（Claude）——对 codex 实施审查的逐项核实

复核人基线：RCoder `e70b3f33`（工作树干净，仅本 reviews/ 目录未跟踪）；实现提交 `3db17134`；build-agent-docker HEAD `4a9897b`（Turso/RBD 改动全部在工作树）。复核只读生产源码 + 临时探针（跑完即删，未提交、未改生产实现）。

**总结论：R01/R02/R03/R05/R06 成立（其中 R01、R03、R04-队列、R05 已用聚焦探针复现），R04、R07 部分成立。审查的六个缺陷项没有一条被推翻；其中 R01 的忙循环 + 线程泄漏、R05 的旧目录静默建二库比审查表述的还要确定（均有运行证据）。**

## 复核方法与探针证据

临时探针文件 `crates/rcoder-storage/src/userapp_lifecycle/turso/r0x_probes.rs`（+ mod 声明一行），每个探针断言**期望的正确行为**，在当前实现上失败即证明缺陷。完整源码见附录 A，可作下一轮回归测试底稿。

```
cargo nextest run -p rcoder-storage --all-features -E 'test(probe_r)' --no-fail-fast
→ Summary: 6 tests run: 0 passed, 6 failed（探针全部按预期暴露缺陷）
```

逐个输出（节选）：

| 探针 | 实测输出 | 含义 |
|---|---|---|
| probe_r01_watch_close_hot_spins | `521217 ready iterations in 300ms` | watch sender 未发 true 即丢弃 + biased `changed()` 在前 ⇒ Err 分支持续就绪，热自旋 |
| probe_r01_quarantine_failure_leaks_spinning_worker | open_exclusive Err 后本进程仍持有 `leak.turso.db` 与 `leak.turso.db-wal` 两个 fd | quarantine 失败路径 worker 线程未退出（泄漏且忙循环） |
| probe_r03_concurrent_shutdown_waits_for_same_completion | `second shutdown returned Ok=true in 2.458µs`（在途 600ms 任务仍在排空） | 并发 shutdown 第二个调用未等同一完成结果 |
| probe_r03_worker_panic_must_not_report_success | `run() after panic -> Err(...)`；`shutdown after panic -> Ok(())` | worker 线程 panic 被 join 结果丢弃吞掉 |
| probe_r04_queue_full_has_bounded_failure | `overflow outcome in 503.00275ms: false` | 队列满后第 257 个请求 500ms 内无任何结果——`send().await` 无界等待 |
| probe_r05_old_sqlite_directory_must_reject | `open on legacy dir -> true` | 目录只有旧 userapp.sqlite3 时静默打开成功并新建 Turso 库 |

探针跑完后 `git checkout -- turso/mod.rs` + 删除探针文件，工作树已还原（`git status` 仅剩本 reviews/ 目录）。

---

## R01：初始化失败/取消可遗留 worker；忙循环 —— **成立（探针复现）**

源码（当前行号）：`crates/rcoder-storage/src/userapp_lifecycle/turso/mod.rs`
- `open_exclusive` 61-82：quarantine（70-78 `?`）失败时，局部 `store`（worker=None、_instance_lock=None）与 `handle` 直接丢弃——store 的 Drop（729-739）因 `worker=None` 发不出 shutdown 信号。
- worker 主循环 111-130：`biased;` 下 `_ = shutdown_rx.changed()` 分支在前（114-122）。`WorkerHandle` 被**未 send(true)** 丢弃后，watch sender 全部消失 → `changed()` 立即返回 Err 且**持续就绪** → `*shutdown_rx.borrow()` 为 false → 不 break → 回到 select → 再次就绪——**热自旋，线程永不退出**。探针一复现（300ms 52 万次就绪）；探针二在真实 quarantine 失败路径上确认线程存活并持有 db/WAL fd。
- 初始化 future 取消同样触发：spawn_worker 的 ready 元组同时携带 `tx.clone()` 与 `shutdown_tx`（100-109）；调用方取消导致 `ready_tx.send` 失败时两者一起丢弃 → 同一忙循环。`drop(ready_tx.send(...))`（102/106）吞掉发送失败。
- init_connection 失败路径（105-108）线程 `return`，不进主循环——该子路径无线程泄漏，但同样未 join、锁（open_exclusive 局部 `lock`）在 `?` 返回时立即释放，可能先于线程内连接 Drop。
- 正常 Drop 兜底（729-739）：发信号**不 join**，随后 `_instance_lock` 字段释放——worker 仍要排空最多 256 个已接收任务，期间第二实例可获取同一目录锁。"锁覆盖连接生命周期"（plan §3：锁归 worker 持有直到连接销毁）在 Drop 路径与错误路径均被打破。显式 `shutdown()` 路径有 join，正确。
- 附带成立：89-95 行 runtime build 的 `.expect(...)` 在 worker 线程 panic（就绪通道随之丢失，调用方得到 `unavailable`，可接受但 panic 信息丢失且属生产代码 expect，违反本仓规则）。

触发时序：quarantine/迁移后置校验失败（如 `turso_invalid_interrupted_record_blocks_startup_without_partial_quarantine` 每次运行都会触发！）、open future 被取消、store 走 Drop 兜底。影响：每次失败泄漏一个 100% CPU 自旋线程 + 活连接 fd；进程内反复重试打开会累积。现有测试已经持续制造该泄漏（测试进程退出才结束），此前未被察觉。

最小修复方向：① `changed()` 的 Err 视同终止信号（sender 消失 ⇒ break）；② 锁所有权移入 worker（线程退出前 Drop），open_exclusive 失败/取消路径显式发信号并 join 回收；③ ready 发送失败即终止循环；④ expect 改错误传播。

## R02：关库前未等业务执行者收束 —— **成立（源码确认，尚未复现）**

源码：`crates/rcoder/src/shutdown.rs:104-114`——收到广播后**第一件事**就是 `userapp_store_control.shutdown()`。而：
- HTTP server（`crates/rcoder/src/server.rs:26-56`）：广播只 break accept 循环；每连接 handler 是独立 `tokio::spawn`，无人 join——收到 SIGTERM 时在途请求（含提交终态的协调调用）仍在跑。
- 恢复扫描器（`crates/rcoder/src/userapp_builder/recovery.rs:63-96`）：独立 spawn 的 interval 循环，**未订阅 shutdown 信号**（select 只有 tasks.next()/interval.tick()），直到进程退出都在产生新的存储工作。
- `main.rs:355-370`：`server_handle.abort()` 在 graceful_shutdown **之后**，数据库先于 HTTP server 关闭。
- plan §3 明确要求反序："停止时先停止业务生产者与恢复扫描器产生新工作，再关闭数据库队列……"。当前实现与之相反。
- PG 同受影响：`postgres.rs` 的 control = `pool.close()`，同样在在途请求收束前关闭。

影响定级（比审查表述更精确）：Turso worker 关闭会排空**已入队**任务，之后的新 `run()` 得到 "worker stopped" Storage 错误——在途协调任务的终态提交失败。数据完整性有恢复兜底（操作停在 Running/未终态 → 下次启动 quarantine 转 RecoveryRequired，不确定性保护仍在），但这是本轮接线**新引入**的可用性缺陷：切换前 sqlite 后端无显式关库，池随 Arc 自然存活到任务结束，不存在此失败模式。

最小修复方向：graceful_shutdown 顺序改为——停 HTTP 接单并等在途连接（有界）、通知并等恢复/清理生产者退出、等在途协调任务（可用受控句柄集合）、最后 `control.shutdown()`；超预算记录未收束项。Turso/PG 都要过。

未验证范围：未做端到端 SIGTERM+在途协调复现（需完整装配），标"源码确认、尚未复现"。

## R03：并发 shutdown 提前成功；panic 被吞 —— **成立（探针复现）**

源码：`turso/mod.rs:181-205`。第二个调用 `take()` 得 None → 立即 `Ok(())`（188-190），不等第一个的 join（探针实测 2.46µs 返回，worker 仍在排空 600ms 任务）——违反 trait-design §6"shutdown 重复调用应安全，**共享实际关闭结果**"。`drop(join.join())`（197-201）丢弃 Result：探针注入 worker panic 后 `shutdown()` 返回 `Ok(())`。另有一个源码确认、未复现的窗口：第一个 shutdown future 在 take 之后、`send(true)` 之前被取消 → handle 永久丢失，此后无人能再触发关机（worker 继续服务，Drop 兜底也找不到 handle），进程退出时线程随 runtime 非正常终止。

最小修复方向：共享 `Running/Closing/Closed` 状态 + 完成通知（oneshot/Notify），所有调用等待同一结果；join 的 Err/panic 显式传播；取消安全（关闭动作不依赖首个调用方 future 存活）。

## R04：延迟回滚与队列无界等待 —— **部分成立（队列项探针复现；数据安全项不成立）**

逐子项：
1. **"错误路径依赖 dangling 延迟回滚不满足方案"——方案符合性成立，数据安全不成立。** Cargo.lock 确认 `turso 0.8.0-pre.11`（crates.io）。依赖源码（本机 registry）`transaction.rs:228` Drop 记 `dangling_tx=Rollback`、`connection.rs:101 maybe_handle_dangling_tx` 在**该连接下一次任意语句前**先执行 ROLLBACK、`transaction.rs:289` `transaction_with_behavior` 同样先处理——"下一请求开始前已回滚"这一点成立（worker 单连接串行）。既有反例 `turso_cancelled_caller_and_dangling_transaction_do_not_leak` + `turso_failure_midway_rolls_back_entire_admission` 覆盖了错误路径数据不残留。但 plan §3 原文是"**所有错误路径显式处理 rollback**，保留主错误与清理错误上下文；API 支持 Drop 不代表回滚已完成"——实现（`exec.rs:107-120` Tx 无 rollback，107-117 文档以 dangling 语义辩护）与已确认方案相悖；且"回滚失败 ⇒ 后端不可写隔离状态"未实现：`maybe_handle_dangling_tx` 失败时 dangling 标志不清除，后续每条语句都重试 ROLLBACK 并失败——事实上的持续写阻止（比审查说的"没有"略好），但无显式隔离状态、错误不分类，无法与一般存储故障区分。
2. **队列满无有界失败——成立（探针复现）。** `run()` 用 `send().await`（mod.rs:162-168），满时无限等容量；`QUEUE_DEPTH` 注释"满时明确报错"（20-21 行）与 run 文档"try_send/send 满 → 明确错误"（153 行）均与实现不符。plan 验收矩阵要求"队列满 → 有界等待"。探针：256 个慢任务占满后，第 257 个 500ms 无结果。

最小修复方向：统一事务执行器显式 begin/body/commit-or-rollback（保留双错误上下文；连接状态不确定时置后端不可写）；`send` 改 `try_send`+明确错误，或带预算的容量等待（不得对已入队任务宣称"未执行"）；修正两处注释。

## R05：旧库目录保护缺失 —— **成立（探针复现 + 生产环境已实际发生）**

spec 行为要求 7 与 plan §2.2 均要求"新库不存在且旧库存在 → fail-fast 拒绝并指引独立目录"。当前 `config/userapp_storage.rs:111-138` 与 `turso/mod.rs:61-82` 的打开链路**没有任何 userapp.sqlite3 检测**（全仓 grep 无命中）。探针：目录仅放一个 dummy `userapp.sqlite3` → `open_exclusive` 成功并新建 Turso 库。且**本地 dev 环境已实际发生该场景**：`docker/data/rcoder/` 中旧 `userapp.sqlite3`（5.7MB，含历史应用身份）与新建 `userapp.turso.db` 并存（verification.md "环境事实"节自己记录了这一点，当时误读为合规）。我此前在 verification.md 写的"指向旧文件时迁移 fail-fast"只是"turso 路径直指旧 sqlite 文件"的情形，不是 spec 要求的同目录并存防护。

影响：同一运行环境静默出现第二份应用身份，旧库中的在途操作/租约/资源绑定语义上被"当作不存在"——正是 spec 点名禁止的。

最小修复方向：在独占锁内、建库前检查默认旧库文件名存在且新库不存在 → 报错并指引独立目录；旧文件内容不动。反例已在探针中（附录 A probe_r05）。

## R06：配套仓库未完整同步；报告超出实际 —— **成立**

build-agent-docker 工作树（未提交）核对结果，plan §6 配套清单 vs 实际：

| plan §6 要求 | 实际 |
|---|---|
| `build_config/rcoder/start-services.sh` | **未改**：5-12 行仍是 `backend=sqlite` 分支 + `RCODER_USERAPP_SQLITE_PATH`；turso 模式下目录可写性 fail-fast 预检整体被跳过 |
| `docker/docker-compose.yml` | 已改（工作树）：env → turso |
| `docker/SQLITE.md` | **未改**，仍是旧 SQLite 文档 |
| `docker-userapp-computer/docker-compose.yml` | 已改（工作树） |
| `docker-userapp-computer/SQLITE.md` | 已改（工作树，→TURSO.md） |
| 隐藏 env 示例 | rcoder `docker/.env.turso.example` 已改；build-agent-docker **`docker/.env.sqlite.example` 未改**（docker-userapp-computer/ 下已改） |
| named-volume 覆盖 | docker-userapp-computer 已改；build-agent-docker **`docker/docker-compose.sqlite-volume.yml` 未改**（旧名仍在） |

另：verification.md T3 写"SQLITE.md→TURSO.md（两仓）"与"start-rcoder.sh 预检块改 Turso"——前者对 `docker/SQLITE.md` 不成立，后者只改了 rcoder 的 dev 脚本 `docker/start-rcoder.sh`，未涵盖 plan 点名的生产脚本 `build_config/rcoder/start-services.sh`（文档也未提及该项遗漏）。审查对"verification 描述不准确"的判断成立。两仓改动均未提交，`3db17134` 不含这些文件（审查表述正确）。

最小修复方向：按上表补齐 4 处；verification.md 勘误。

## R07：测试覆盖与证据表述 —— **部分成立**

- **取消测试覆盖缺口：成立。** `turso/mod.rs` `turso_cancelled_caller_and_dangling_transaction_do_not_leak` 取消的是 sleep body；写事务错误路径由另一个 dangling 场景覆盖。"调用方取消发生在真实写事务中段"无直接反例。组合语义上两者等价（oneshot drop 不影响 body 执行），但按 AGENTS"覆盖真实调用链关键时序"应补一条取消时 body 正在做写+后继 commit 的用例。
- **Compose 8 失败归因严谨性：成立。** verification.md 对 config_hash 写"疑为"往返差异，未做同条件基线复跑（有三边佐证：镜像/源码版本一致、比对大小写不敏感、本轮 diff 不含该路径，但无基线对照）。AGENTS 要求"未复现则标为待归因"——tasks.md 却勾选了"完整 test-e2e"项（勾选文案披露了失败与归因，但按"验证未通过不得勾完成"的严格口径属超前）。K8s 全绿与 131 复测（0.1.274 配对镜像后 userapp 51 场景全过）旁证了非 Turso 根因，但不构成同条件基线。
- **"M4 冲突缺 blocker.scope 不能降低测试预期"：前提不成立（本轮未降低任何测试预期——`3db17134` 未触碰 compose_userapp*/scope_isolation/deploy 套件，已核对 diff），但契约问题本身成立待裁决**：M3（664a53de）确立"结构化 blocker 透传，免解析消息文本"契约，而 M4 进程锁层 409 信封 data=null 与之冲突。方向应是修锁层透传或明确豁免，不是改测试。
- **RBD/STS 分离：已正确执行。** rcoder 侧 RBD verification 明确标注集群门禁未运行；K8s 全绿未被宣称为 chart 迁移验收。审查也认可，无新增问题。

---

## 最终总结

### 1. 确认成立且仍需修复（优先级序）

1. **R01（P1）** worker 忙循环 + 线程/fd 泄漏 + 锁所有权：`turso/mod.rs:111-130`（changed Err 持续就绪）、`61-82`（错误路径 handle 丢弃）、`729-739`（Drop 不 join 即释放锁）。现有损坏记录测试每次运行都在触发泄漏。
2. **R02（P1）** 关机顺序颠倒：`shutdown.rs:104-114` 先关库；恢复扫描器（`recovery.rs:63-96`）无 shutdown 接入；HTTP 在途连接不 join；PG 同受影响。本轮新引入的失败模式。
3. **R05（P2）** 旧库目录静默建二库：`config/userapp_storage.rs:111-138` 无检测；dev 环境已实际发生双库并存。
4. **R03（P2）** shutdown 结果不共享（2.46µs 假 Ok）、panic 吞掉、cancel 窗口丢句柄：`turso/mod.rs:181-205`。
5. **R04（P2，部分）** 队列满无界等待（`mod.rs:162-168` + 两处注释不符）；错误路径改显式 rollback + 不可写隔离状态（`exec.rs:107-120`）。
6. **R06（P2）** build-agent-docker 4 处未同步 + verification.md 勘误（start-services.sh / docker/SQLITE.md / docker/.env.sqlite.example / docker/docker-compose.sqlite-volume.yml）。

### 2. 不成立或已缓解的指控及依据

- R04 "半事务可能提交/数据残留"：不成立——turso 0.8.0-pre.11 源码（Cargo.lock 确认）dangling_tx 在连接下一语句前必然回滚；两个既有反例验证了数据不残留。成立的只是方案符合性（显式 rollback/隔离状态）与注释失实。
- R04 "回滚失败后不阻止新写"：部分缓解——dangling 标志不清除导致后续语句全部持续失败（事实阻止），但无显式状态与错误分类。
- R07 "降低 M4 测试预期"：本轮未发生（diff 核对）；契约冲突本身待裁决。
- R07 "RBD/普通 K8s 混淆"：未发生，记录已分离。

### 3. 实际测试证据与未验证范围

已运行（命令 + 结果见"复核方法"表）：
- `cargo nextest run -p rcoder-storage --all-features -E 'test(probe_r)' --no-fail-fast` → 6 探针全部按预期失败（退出码非 0），逐项输出已录。
- 复核过程中未运行其他新测试；未跑 Compose/PG/K8s（引用 verification.md 已有记录，未重新验证）。

未验证/标记范围：
- R02 端到端 SIGTERM+在途协调复现——源码确认，尚未复现。
- R03 cancel-before-send 窗口——源码确认（确定性注入困难），尚未复现。
- R01 初始化取消（ready send 失败）路径——机制探针 + 源码确认，未单独复现真实取消。
- 8 个 Compose 失败的同条件基线（旧构建 + 同环境重跑）——未执行。

### 4. 建议下一轮修复的文件与回归用例

| 修复 | 文件 | 回归（附录 A 探针可直接改造） |
|---|---|---|
| worker 终止语义 + 锁所有权 | `turso/mod.rs`（主循环 111-130、open_exclusive 61-82、Drop 729-739、spawn_worker 84-150） | probe_r01 两个（自旋计数 + fd 泄漏）+ 新增"open 取消后第二实例必拒/必过" |
| 关机顺序 | `shutdown.rs`、`main.rs`、`server.rs`、`recovery.rs`（扫描器接 shutdown）、`background_tasks.rs` | SIGTERM 屏障反例：在途 runtime 写释放后终态能提交；Turso+PG 双后端 |
| shutdown 结果共享 | `turso/mod.rs:181-205`（Turso+PG control 两处） | probe_r03 两个 + cancel-首调反例 |
| 显式 rollback/隔离 + 队列有界 | `turso/exec.rs`、`turso/mod.rs:151-178`、`QUEUE_DEPTH` 注释 | probe_r04（有界失败）+ "回滚失败后下一业务不执行且错误可分类" |
| 旧库目录拒绝 | `config/userapp_storage.rs` 或 `turso/mod.rs` 打开链 | probe_r05（拒绝且不产生新文件；空目录成功；已有新库正常打开） |
| 配套同步 + 勘误 | build-agent-docker `build_config/rcoder/start-services.sh`、`docker/SQLITE.md`、`docker/.env.sqlite.example`、`docker/docker-compose.sqlite-volume.yml`；rcoder `verification.md` 勘误 | 三份 Compose 配置解析（已有门禁）+ 生产脚本 `bash -n` + 不可写目录 fail-fast |

修复顺序建议：R01 → R02（两者都在生命周期链上，改动相邻）→ R05（最小、独立）→ R03 → R04 → R06；每步先落附录 A 对应探针为正式回归，再改实现。

---

## 附录 A：探针源码（已从工作树删除，全文备查）

```rust
// crates/rcoder-storage/src/userapp_lifecycle/turso/r0x_probes.rs
// 声明：turso/mod.rs 内加 #[cfg(test)] mod r0x_probes;
#![cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use shared_types::UserAppStoreError as Error;
use super::{TaskOut, TursoUserAppStore};

fn temp_db(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("dir");
    let path = std::path::absolute(dir.path().join(name)).expect("abs");
    (dir, path)
}

/// R01 机制探针：worker 主循环的精确形状（biased changed() 在前 +
/// sender 未发 true 即丢弃）→ 断言不热自旋。实测 300ms 521217 次就绪。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_r01_watch_close_hot_spins() {
    let spins = Arc::new(AtomicU64::new(0));
    let handle = {
        let spins = spins.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all().build().expect("rt");
            rt.block_on(async move {
                let (tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
                let (_task_tx, mut rx) = tokio::sync::mpsc::channel::<()>(8);
                drop(tx);
                let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
                loop {
                    tokio::select! {
                        biased;
                        _ = shutdown_rx.changed() => {
                            spins.fetch_add(1, Ordering::Relaxed);
                            if *shutdown_rx.borrow() { break; }
                        }
                        task = rx.recv() => { assert!(task.is_none()); break; }
                    }
                    if tokio::time::Instant::now() >= deadline { break; }
                }
            });
        })
    };
    handle.join().expect("probe thread");
    let count = spins.load(Ordering::Relaxed);
    println!("R01 spin probe: {count} ready iterations in 300ms");
    assert!(count < 10_000, "changed() Err 分支持续就绪造成热自旋：300ms 内 {count} 次就绪");
}

/// R01 真实路径探针：quarantine 失败 → open_exclusive Err 后，
/// 本进程仍持有 db/WAL fd（worker 泄漏且忙循环）。实测 2 个 fd。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_r01_quarantine_failure_leaks_spinning_worker() {
    use shared_types::UserAppLifecycleStore as _;
    let (dir, path) = temp_db("leak.turso.db");
    let store = TursoUserAppStore::open_exclusive(&path).await.expect("open");
    let request = shared_types::UserAppAdmission {
        runtime_policy_on_success: None, command: None, metadata: None,
        app_id: "leak-app".into(), lifecycle_id: None,
        operation_id: "leak-op".into(), request_id: Some("leak-req".into()),
        request_fingerprint: "a".repeat(64),
        kind: shared_types::UserAppOperationKind::Update,
    };
    let op = match store.admit(&request).await.expect("admit") {
        shared_types::UserAppAdmissionOutcome::Accepted(op) => op,
        shared_types::UserAppAdmissionOutcome::Existing(op) => op,
    };
    let progress = shared_types::UserAppOperationProgress {
        app_id: op.app_id.clone(), operation_id: op.operation_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(), expected_revision: op.revision,
        executor_id: "w".into(), state: shared_types::UserAppOperationState::Running,
        step: "s".into(), checkpoint: serde_json::Value::Null,
        error_code: None, error_message: None,
    };
    store.advance(&progress).await.expect("running");
    store.shutdown().await.expect("shutdown");
    drop(store);
    let db = turso::Builder::new_local(path.to_string_lossy().as_ref())
        .build().await.expect("engine");
    let conn = db.connect().expect("conn");
    conn.execute(
        "UPDATE userapp_operations SET record='{\"state\":\"Running\",\"broken\":true}' \
         WHERE operation_id='leak-op'", ()).await.expect("corrupt");
    drop(conn); drop(db);
    let result = TursoUserAppStore::open_exclusive(&path).await;
    assert!(result.is_err(), "损坏记录应阻断启动");
    std::thread::sleep(Duration::from_millis(300));
    let pid = std::process::id();
    let lsof = std::process::Command::new("lsof")
        .args(["-p", &pid.to_string(), "-F", "n"]).output().expect("lsof");
    let out = String::from_utf8_lossy(&lsof.stdout);
    let db_name = path.file_name().unwrap().to_string_lossy().to_string();
    let holding: Vec<&str> = out.lines()
        .filter(|l| l.starts_with('n') && l.contains(&db_name)).collect();
    println!("R01 leak probe: open fd(s) on {db_name} after failed open: {holding:?}");
    assert!(holding.is_empty(),
        "quarantine 失败后 worker 线程应退出并释放连接；实际仍持有 {} 个 fd", holding.len());
    drop(dir);
}

/// R03 探针：并发 shutdown 第二个调用必须等待同一完成结果。
/// 实测 2.458µs 提前返回 Ok（600ms 任务仍在排空）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_r03_concurrent_shutdown_waits_for_same_completion() {
    let (_dir, path) = temp_db("shutdown.turso.db");
    let store = Arc::new(TursoUserAppStore::open_exclusive(&path).await.expect("open"));
    let slow = Arc::clone(&store);
    tokio::spawn(async move {
        let _: Result<String, Error> = slow
            .run(|_conn: &mut turso::Connection| {
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    Ok(TaskOut(Box::new("slow-done".to_string())))
                })
            }).await;
    });
    tokio::time::sleep(Duration::from_millis(80)).await;
    let first = { let s = Arc::clone(&store);
        tokio::spawn(async move { s.shutdown().await }) };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = Instant::now();
    let second = Arc::clone(&store).shutdown().await;
    let second_elapsed = started.elapsed();
    println!("R03 probe: second shutdown returned Ok={:?} in {second_elapsed:?}", second.is_ok());
    first.await.expect("first join").expect("first shutdown");
    assert!(second_elapsed >= Duration::from_millis(300),
        "第二个 shutdown 在 {second_elapsed:?} 内提前返回——未等待 worker 实际关闭完成");
}

/// R03 探针：worker panic 后 shutdown 不得 Ok。实测 shutdown -> Ok(())。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_r03_worker_panic_must_not_report_success() {
    let (_dir, path) = temp_db("panic.turso.db");
    let store = TursoUserAppStore::open_exclusive(&path).await.expect("open");
    let result: Result<String, Error> = store
        .run(|_conn: &mut turso::Connection| {
            Box::pin(async move { panic!("probe: injected worker panic"); })
        }).await;
    println!("R03 probe: run() after panic -> {result:?}");
    let shutdown = store.shutdown().await;
    println!("R03 probe: shutdown after panic -> {shutdown:?}");
    assert!(shutdown.is_err(), "worker 线程 panic 后 shutdown 不得返回 Ok（join 结果被丢弃）");
}

/// R04 探针：队列满（256 慢任务）后第 257 个请求应有界失败。
/// 实测 500ms 无任何结果（send().await 无界等待）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_r04_queue_full_has_bounded_failure() {
    let (_dir, path) = temp_db("queue.turso.db");
    let store = Arc::new(TursoUserAppStore::open_exclusive(&path).await.expect("open"));
    let mut tasks = Vec::new();
    for _ in 0..256 {
        let s = Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            let _: Result<String, Error> = s
                .run(|_conn: &mut turso::Connection| {
                    Box::pin(async move {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok(TaskOut(Box::new("q".to_string())))
                    })
                }).await;
        }));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = Instant::now();
    let overflow = tokio::time::timeout(Duration::from_millis(500), {
        let s = Arc::clone(&store);
        async move {
            let _: Result<String, Error> = s
                .run(|_conn: &mut turso::Connection| {
                    Box::pin(async move { Ok(TaskOut(Box::new("overflow".to_string()))) })
                }).await;
        }
    }).await;
    let elapsed = started.elapsed();
    println!("R04 probe: overflow outcome in {elapsed:?}: {:?}", overflow.is_ok());
    assert!(overflow.is_ok(), "队列满时第 257 个请求应在 500ms 内得到明确结果；实际无限等待");
    for t in tasks { let _ = t.await; }
    store.shutdown().await.expect("cleanup shutdown");
}

/// R05 探针：目录只有旧 userapp.sqlite3 时必须拒绝。实测静默打开成功。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_r05_old_sqlite_directory_must_reject() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(dir.path().join("userapp.sqlite3"),
        b"SQLite format 3\x00dummy-legacy").expect("old db");
    let path = std::path::absolute(dir.path().join("userapp.turso.db")).expect("abs");
    let result = TursoUserAppStore::open_exclusive(&path).await;
    println!("R05 probe: open on legacy dir -> {:?}", result.is_ok());
    assert!(result.is_err(), "旧库存在且新库不存在时应 fail-fast 拒绝启动");
    assert!(!path.exists(), "拒绝路径不得创建新库文件");
}
```

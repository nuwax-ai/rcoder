# DevServerManager RAII Guard 设计参考

> 目标读者：负责在 `crates/file-server` 落地 RAII 的实现 agent。
> 前提结论：**`ViteManager` 已存在，就是 `DevServerManager`，无需重写。** 本文档只补 RAII guard 的正确落点。
>
> **状态（已落地）**：落点一（`shutdown_all` + `impl Drop` + main graceful shutdown）与落点三（`AllocGuard::disarm`）已实现并合入；落点二（`DevHandle`）当前无调用方，按 YAGNI 暂不引入；utoipa OpenAPI 文档化已全 crate 落地。下文代码块为**实际实现**（已修复初稿中 graceful shutdown 信号未 await 的 bug）。

---

## 0. 先读这几处（现状速览）

| 位置 | 已有能力 |
|---|---|
| `src/service/dev_server/mod.rs` | `DevServerManager`：`start_dev` / `stop_dev` / `restart_dev` / `keep_alive` / `list_dev` / `port_pool_status` / `read_dev_log`；`Mutex<HashMap<String, DevProcess>>` 实例表 + `Mutex<HashSet<String>>` 启动锁 + `PortPool` |
| `src/service/dev_server/process.rs` | `spawn_dev`(detached + `process_group(0)`)、`kill_process_group`/**同步**、`kill_process_group_force`/**同步**、`wait_for_stop`/async、`find_pids_by_project_id`/async、`is_project_alive`/async、`is_process_running`/同步 |
| `src/service/dev_server/port_pool.rs` | `PortPool`：`allocate`(幂等复用)/`release`/`status` |
| `mod.rs` 内 `StartingGuard` / `AllocGuard` | 已有的两个**局部栈 guard**（drop 清启动锁 / drop 归还端口） |
| `src/main.rs` | `axum::serve(listener, app).await?` —— **无 graceful shutdown，无 manager Drop** |
| `src/error.rs` | `AppError`/`AppResult`，`lock()` helper 见 `mod.rs:414` |

---

## 1. 设计张力（为什么不能照搬 axum-vite / vite-rs）

| 维度 | axum-vite / vite-rs | rcoder 现状 |
|---|---|---|
| 进程句柄 | **持有 `Child`**，`kill_on_drop`，guard drop 即 kill | **`drop(child)` 丢弃句柄**，detached 独立存活 |
| 实例生命周期 | guard 生命周期 = dev server 生命周期（请求/作用域级） | dev server **跨请求长期存活**（HTTP 触发 start，函数返回后进程继续） |
| 停止方式 | drop Child | 靠 `pid` + `kill(-pgid)` |
| 集中管理 | 单实例 | 多实例 Map |

**结论**：rcoder 的 detached 模型是**有意为之**（start 是 HTTP 请求触发的，`start_dev` 函数返回时不能让 Child 被 drop 掉把进程杀掉）。所以 axum-vite 那种"持 Child、drop 即停"的 guard **不能直接套**。

RAII 在本模型下有 **3 个正确落点**，下面逐个给出。

---

## 2. 落点一（最重要）：Manager 级 `shutdown_all` + `impl Drop`

### 问题
- `DevServerManager` **没有 `Drop`**，`main.rs` 也无 graceful shutdown。
- file-server 进程退出时，所有 detached 的 dev server 变成**孤儿进程**（端口/资源泄漏到容器或宿主）。

### 方案：主路径异步全量清理 + Drop 同步兜底

> 关键约束：`Drop::drop` **不能 `.await`**。nix 的 `kill_process_group` 是**同步**的，所以 Drop 里可以发信号、清 Map、还端口；但不能做 `wait_for_stop`(async sleep) / `find_pids_by_project_id`(async ps)。因此“完整的 SIGTERM→等→SIGKILL 升级”放到异步的 `shutdown_all`，Drop 只发 SIGTERM+SIGKILL 兜底。

`src/service/dev_server/mod.rs`：

```rust
use std::sync::Arc;

impl DevServerManager {
    /// 全量优雅停止：逐个项目走完整 stop_dev 流程
    /// (SIGTERM → wait → SIGKILL 升级 + ps 扫描兜底 + 还端口 + 清日志)。
    /// 供 main.rs graceful shutdown 调用。
    pub async fn shutdown_all(&self) {
        let snapshot: Vec<String> = lock(&self.processes)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        if snapshot.is_empty() {
            return;
        }
        tracing::info!("shutdown_all: stopping {} dev server(s)", snapshot.len());
        for project_id in snapshot {
            // stop_dev 内部幂等：进程已不在也会安全返回
            if let Err(e) = self.stop_dev(&project_id).await {
                tracing::warn!(%project_id, "shutdown_all stop failed: {e}");
            }
        }
    }
}

/// Drop 兜底：进程正常 graceful shutdown 已由 `shutdown_all` 清理；此处只防
/// "panic / Arc 提前释放 / shutdown_all 未触发" 残留。
///
/// 约束：`Drop::drop` 不能 `.await`，无法给 SIGTERM grace 宽限期 —— 发 SIGTERM 后
/// 无等待地 SIGKILL 等价于直接 SIGKILL，故此处省去无意义的 SIGTERM，直接对进程组
/// SIGKILL，确保进程终止并还端口、清表。
impl Drop for DevServerManager {
    fn drop(&mut self) {
        let Ok(procs) = self.processes.get_mut() else { return };
        if procs.is_empty() {
            return;
        }
        tracing::warn!(
            "DevServerManager dropped with {} live dev server(s) — best-effort SIGKILL",
            procs.len()
        );
        for (project_id, p) in procs.iter() {
            // 兜底硬杀：SIGKILL 进程组（无法 await，故不走 SIGTERM→等→SIGKILL 升级）
            if !process::kill_process_group_force(p.pid) {
                tracing::warn!(%project_id, pid = p.pid, "SIGKILL failed in Drop");
            }
            self.port_pool.release(project_id);
        }
        procs.clear();
    }
}
```

> 注意：`processes` 当前是 `Mutex<HashMap<..>>`。Drop 里用 `get_mut()`（无需加锁，借用独占）。若后续改成其它锁类型，相应调整。`port_pool.release` 内部自带锁，OK。
>
> ⚠️ **Drop 不覆盖的场景**：file-server 自身被 SIGKILL 强杀时进程直接终止，**Drop 不会执行**，detached 的 dev server 仍会成孤儿。这是 detached 模型的固有局限，只能靠容器/编排层（Pod 退出回收）兜底。正常重启走 SIGTERM → `shutdown_all` 路径已覆盖。

`src/main.rs`：接 graceful shutdown 信号。**关键：SIGTERM 的 `recv` future 必须真正被 `await`/`select!` poll，不能只构造信号 handler 就返回**（否则 `with_graceful_shutdown` 闭包瞬间完成，等于没等信号就 shutdown）。`state` 会被 `with_state` 消费，故先 clone 出 `Arc<DevServerManager>` 给闭包。

```rust
// state 会被 with_state 消费，先 clone Arc 给 shutdown 闭包
let dev_server = state.dev_server.clone();
let app = Router::<AppState>::new() /* ... */.with_state(state);

axum::serve(listener, app)
    .with_graceful_shutdown(shutdown_signal(dev_server))
    .await?;
Ok(())
}

/// 接收 SIGINT / SIGTERM → 优雅停止所有 dev server。信号 handler 安装失败降级为仅 ctrl_c；
/// 全程无 unwrap/expect（生产规范）。
async fn shutdown_signal(dev_server: Arc<DevServerManager>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "install SIGTERM handler failed, fallback to ctrl_c only");
                let _ = tokio::signal::ctrl_c().await;
                dev_server.shutdown_all().await;
                return;
            }
        };
        // 真正 await 信号到达（recv 必须被 poll）
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("received SIGINT, shutting down dev servers"),
            _ = term.recv() => tracing::info!("received SIGTERM, shutting down dev servers"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("received interrupt, shutting down dev servers");
    }
    dev_server.shutdown_all().await;
}
```

---

## 3. 落点二（可选，暂不实现）：运行实例句柄 `DevHandle`

> **状态**：当前 file-server 无程序化"临时起 dev server 用完即弃"的调用方（HTTP 场景 dev server 跨请求长期存活，不需绑请求作用域）。按 YAGNI 暂不引入。下文设计保留作未来有调用方（脚本/工具/测试）时的参考。

### 适用场景
HTTP 场景里 dev server 跨请求长期存活，**不需要**把句柄绑请求作用域。`DevHandle` 主要用于：
- 单元/集成测试（起一个、断言、drop）
- 程序化“临时起一个 dev server 用完即弃”（脚本/工具）

### 设计：显式 async 停止为主路径，Drop 同步兜底

`src/service/dev_server/mod.rs`：

```rust
/// 一个运行中 dev server 的拥有型句柄。
/// 主路径：显式 `stop().await`（走完整异步清理）。
/// Drop 兜底：尽力同步 kill（防止泄漏），但拿不到 pid→project 反查时只发信号。
pub struct DevHandle {
    mgr: Arc<DevServerManager>,
    project_id: String,
    pid: u32,
    port: u16,
}

impl DevHandle {
    /// 起一个 dev server 并返回句柄。drop 自动停止（尽力）。
    pub async fn start(
        mgr: Arc<DevServerManager>,
        project_id: impl Into<String>,
        project_path: &Path,
        base_path: Option<&str>,
    ) -> AppResult<Self> {
        let project_id = project_id.into();
        let started = mgr.start_dev(&project_id, project_path, base_path).await?;
        Ok(Self { mgr, project_id, pid: started.pid, port: started.port })
    }

    pub fn pid(&self) -> u32 { self.pid }
    pub fn port(&self) -> u16 { self.port }

    /// 显式停止（主路径）：完整 stop_dev（SIGTERM→等→SIGKILL + ps 兜底 + 还端口 + 清日志）。
    pub async fn stop(self) -> AppResult<StoppedDev> {
        self.mgr.stop_dev(&self.project_id).await
    }
}

impl Drop for DevHandle {
    fn drop(&mut self) {
        // 不能 await → 只能同步 kill。先尽力杀进程，再尝试 async 清理的“廉价部分”。
        if process::is_process_running(self.pid) {
            if !process::kill_process_group(self.pid) {
                let _ = process::kill_process_group_force(self.pid);
            }
        }
        // 端口/日志清理依赖 stop_dev(async)，Drop 里跳过；可在此 spawn 一个尽力任务：
        let mgr = self.mgr.clone();
        let pid = self.project_id.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = mgr.stop_dev(&pid).await; // 幂等：进程已死也会还端口/清日志
            });
        }
    }
}
```

> `tokio::runtime::Handle::try_current()` 失败（无 runtime，如纯测试外部）时退化为只发信号——这是 `async-drop` 痛点的标准折中。**调用方应优先 `handle.stop().await`**，Drop 只是安全网。

---

## 4. 落点三：用 `AllocGuard::disarm` 取代 `mem::forget`（已实现）

### 现状（改造前 `mod.rs:146` 与 `:224`）
```rust
let port_alloc = AllocGuard { pool: &self.port_pool, project_id: project_id.to_string() };
...
std::mem::forget(port_alloc); // 成功不释放
```
说明：`std::mem::forget` 是合法 safe API，用来阻止 guard 的 Drop 属文档化用法，并非”错误”。但 `disarm()` 标志位语义更清晰（review 时一眼看出”armed=false 后 Drop 变 no-op”），故采纳。

### 方案：`disarm()` 标志位（已落地）
```rust
struct AllocGuard<'a> {
    pool: &'a PortPool,
    project_id: String,
    armed: bool,
}
impl AllocGuard<'_> {
    fn disarm(&mut self) { self.armed = false; }
}
impl Drop for AllocGuard<'_> {
    fn drop(&mut self) {
        if self.armed { self.pool.release(&self.project_id); }
    }
}
```
调用处：
```rust
let mut port_alloc = AllocGuard { pool: &self.port_pool, project_id: project_id.to_string(), armed: true };
...
port_alloc.disarm(); // 取代 std::mem::forget(port_alloc)
```
语义清晰：armed=false 后 Drop 变 no-op，guard 仍在作用域正常析构。

---

## 5. 对齐 CLAUDE.md 全局 Rust 规范（务必遵守）

| 规范 | 本设计的落点 |
|---|---|
| 单线程/单进程不用 dashmap | `DevServerManager` 用 `Mutex<HashMap>` + `Mutex<HashSet>`，**保持不变**，禁止引入 dashmap |
| 生产禁用 `unwrap()`/`expect()` | 现有生产代码已用 `?`/`AppError`；**Drop 内用 `if let Ok(..)` / `unwrap_or_default`**，禁止 `.unwrap()`。测试代码可 `unwrap` |
| Fail Fast | 启动失败（进程早退/端口耗尽/解析失败）已 Fail Fast 抛 `AppError`；Drop/shutdown 路径的失败 **记 warn 不 panic**（清理路径不能因一个进程失败影响其余） |
| `utoipa` 给 HTTP 接口完备 OpenAPI | ✅ **已落地（全 crate）**：file-server 已引入 `utoipa`/`utoipa-axum`/`utoipa-swagger-ui`，handler 统一加 `#[utoipa::path]`、共享响应体派生 `ToSchema`，路由经 `OpenApiRouter` 聚合并内嵌 Swagger UI（`/api-docs`）。dev server 接口（start-dev/stop-dev/restart-dev/list-dev/keep-alive 等）已纳入 OpenAPI 文档 |
| 禁用 `unsafe` | `nix` 的 `kill`/`Pid` 是安全封装，无裸 `unsafe`，保持现状；新增代码不得引入 `unsafe` |
| SOLID | `DevHandle`/`DevServerManager`/`PortPool` 单一职责已清晰；`shutdown_all`/`Drop` 属 manager 的生命周期职责 |

---

## 6. 验收标准 & 测试要点

1. **Drop 清理**：起 2 个 dev server，`drop(Arc<DevServerManager>)`（强 `Arc::try_unwrap` 或构造时不 clone），断言两个 pid 的进程组已被 SIGTERM，端口池 `status()` 清空。
2. **graceful shutdown**：集成测试用 `tokio::spawn` 跑 main 逻辑 + 发 SIGTERM，断言 `shutdown_all` 被调用且子进程退出。
3. **`DevHandle::stop` vs Drop**：两条路径都验证端口归还 + pid 不可达。
4. **`AllocGuard::disarm`**：成功路径端口不被释放（仍占用），失败路径端口归还。
5. **清理路径健壮性**：人为让一个 pid 失效（已死），`shutdown_all`/Drop 仍能清理其余，不 panic、不中断。
6. **回归**：现有 `port_pool.rs` 单测全绿；新增 `dev_server` 模块单测覆盖 Drop/Handle。

> 测试用例内可用 `unwrap()`/`expect()`（符合规范——测试场景允许）。生产代码（Drop/shutdown_all/handle）禁用。

---

## 7. 参考库映射（借鉴点，不要整体依赖）

| 参考库 | 借鉴什么 | 不借鉴什么 |
|---|---|---|
| `vite-rs` `start_dev_server` → `ViteProcess` RAII | guard 持有句柄、Ctrl-C/signal 处理、drop 即停的**形态** | 它的“Child 句柄 + 单实例”模型（rcoder 是 detached + 多实例） |
| `axum-vite` `maybe_spawn_dev_server` | spawn + 端口注入 + 日志分流 + `--clearScreen false`（**rcoder 已实现**） | 绑 axum router、单 frontend_root |
| `command-group` crate | 跨平台进程组（Win Job Object / Unix pgroup） | rcoder 现用 `nix` + `process_group(0)` 已够 Unix；**若要支持 Windows，引入 `command-group` 替换 `nix` 路径** |

---

## 8. 实现顺序与落地状态

1. ✅ 落点三（`AllocGuard::disarm`）——最小改动，已落地。
2. ✅ 落点一（`shutdown_all` + `impl Drop` + `main.rs` graceful shutdown）——修复孤儿进程泄漏，已落地。graceful shutdown 已验证：SIGTERM 后日志打印 "received SIGTERM, shutting down dev servers" 并干净退出。
3. ⏸ 落点二（`DevHandle`）——无调用方，暂不实现（YAGNI）。
4. ✅ utoipa OpenAPI 文档化——已全 crate 落地（依赖 + `#[utoipa::path]` + `ToSchema` + `OpenApiRouter` + Swagger UI）。

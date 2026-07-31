//! Dev server 进程管理器 (对齐 nuwax `processManager.js` + `startDevUtils.js` 等)。
//!
//! 替换 nuwax 的裸 `child_process.spawn` (detached) + 内存 Map。
//! 用 `tokio::process::Command` (process_group + 丢弃 Child 句柄) + `Mutex<HashMap>`。
//!
//! 注意 (对齐 nuwax 已知宽松行为):
//! - 纯内存状态, 无持久化 (重启即丢)
//! - 存活轮询超时后仍返回成功 (nuwax 行为)
//! - keep-alive 被动探活, 无空闲自动停止

pub mod error_classify;
pub mod log;
pub mod port_pool;
pub mod process;
mod support;

pub use error_classify::{STDERR_RING_CAP, StderrRing, ViteStartupError};
pub use log::{ReadDevLogResult, read_dev_log};
pub use port_pool::{PortAllocation, PortPool, PortPoolStatus};
pub use process::{is_process_running, is_project_alive, kill_process_group, now_ms};

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::Config;
use crate::error::{AppError, AppResult};
use crate::service::pnpm::{self, InstallOptions, LogFiles};
use support::{early_exit_err, ldrtemp, lock, read_dev_script};

/// 探活回调：(port, base_path, timeout_ms) → boxed future<bool>。
/// 抽成类型别名既绕开 clippy::type_complexity，也方便测试注入 stub（绕开 reqwest 延迟）。
type AliveProbe<'a> = &'a (dyn for<'s> Fn(u16, Option<&'s str>, u64)
    -> std::pin::Pin<Box<dyn Future<Output = bool> + Send + 's>>
    + Sync);

/// 运行中的 dev server 记录 (内存状态)。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DevProcess {
    pub pid: u32,
    pub port: u16,
    pub project_id: String,
    pub started_at: i64,
    #[serde(skip)]
    pub log_dir: PathBuf,
    #[serde(skip)]
    pub temp_log_name: String,
}

/// start-dev / restart-dev 响应。
#[derive(Debug, Clone, serde::Serialize)]
pub struct StartedDev {
    pub pid: u32,
    pub port: u16,
}

/// stop-dev 响应。
#[derive(Debug, Clone, serde::Serialize)]
pub struct StoppedDev {
    pub killed_pids: Vec<KilledPid>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct KilledPid {
    pub pid: u32,
    pub killed: bool,
}

/// keep-alive 结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct KeepAliveResult {
    pub alive: bool,
    pub action: Option<String>,
    /// 重启分支返回新启动的 pid/port (对齐 nuwax 透传 startDevServer 返回值);
    /// alive 分支为 None (调用方用查询入参的 pid/port)。
    pub pid: Option<u32>,
    pub port: Option<u16>,
}

/// 启动锁守卫: drop 时自动从 starting 集合移除 (防 early return 泄漏)。
struct StartingGuard<'a> {
    starting: &'a Mutex<HashSet<String>>,
    project_id: String,
}
impl Drop for StartingGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut s) = self.starting.lock() {
            s.remove(&self.project_id);
        }
    }
}

/// dev server 进程管理器 (经 Arc 注入 AppState)。
pub struct DevServerManager {
    processes: Mutex<HashMap<String, DevProcess>>,
    starting: Mutex<HashSet<String>>,
    port_pool: PortPool,
    config: Arc<Config>,
}

impl DevServerManager {
    pub fn new(config: Arc<Config>) -> Self {
        let pool = PortPool::new(
            config.dev_port_range_start,
            config.dev_port_range_end,
            config.dev_port_reserved_start,
            config.dev_port_reserved_end,
        );
        Self {
            processes: Mutex::new(HashMap::new()),
            starting: Mutex::new(HashSet::new()),
            port_pool: pool,
            config,
        }
    }

    /// start-dev (对齐 nuwax startDevServer)。
    pub async fn start_dev(
        &self,
        project_id: &str,
        project_path: &Path,
        base_path: Option<&str>,
    ) -> AppResult<StartedDev> {
        // 启动锁
        {
            let mut starting = lock(&self.starting)?;
            if starting.contains(project_id) {
                return Err(AppError::business(
                    "project dev server is already starting, please wait",
                ));
            }
            starting.insert(project_id.to_string());
        }
        let _guard = StartingGuard {
            starting: &self.starting,
            project_id: project_id.to_string(),
        };
        self.start_dev_inner(project_id, project_path, base_path)
            .await
    }

    async fn start_dev_inner(
        &self,
        project_id: &str,
        project_path: &Path,
        base_path: Option<&str>,
    ) -> AppResult<StartedDev> {
        // 幂等: 已运行则返回现有 pid/port
        if let Some(p) = lock(&self.processes)?.get(project_id).cloned() {
            return Ok(StartedDev {
                pid: p.pid,
                port: p.port,
            });
        }

        let port = self.port_pool.allocate(project_id)?;
        let mut port_alloc = AllocGuard {
            pool: &self.port_pool,
            project_id: project_id.to_string(),
            armed: true,
        };

        let ldir = log::log_dir(&self.config, project_id);
        tokio::fs::create_dir_all(&ldir)
            .await
            .map_err(|e| AppError::system(format!("create dev log dir: {e}")))?;
        let now = now_ms();
        let main_log = ldir.join(log::main_log_name());
        let temp_log = ldrtemp(&ldir, now);

        self.write_npmrc(project_path).await?;

        // 命令: override env (运维控制, 非用户输入 → sh -c 安全) 优先;
        // 否则从 package.json 读 dev script → arg 数组 (用户输入经此路径, 避免注入)
        let ovr = std::env::var("DEV_SERVER_OVERRIDE_CMD")
            .ok()
            .filter(|s| !s.trim().is_empty());
        // dev-inject / design-mode 注入 (业务需要: 让前端可用 design 模式);
        // 失败仅记日志不阻塞 dev server 启动 (对齐 nuwax processManager 的 set +e 宽松语义)。
        if let Err(e) = process::run_command_to_log(
            "sh",
            &[
                "-c",
                "set +e; pnpm dlx @xagi/dev-inject@latest install --framework; pnpm dlx @xagi/vite-plugin-design-mode@latest install; set -e",
            ],
            project_path,
            &main_log,
            &temp_log,
            self.config.dev_command_timeout_secs,
            None,
        )
        .await
        {
            tracing::warn!(error = %e, "dev-inject/design-mode preCmd failed (non-blocking)");
        }

        let (child, stdout, stderr) = match ovr {
            Some(ovr) => {
                let cmd = ovr
                    .replace("{PORT}", &port.to_string())
                    .replace("{BASE}", base_path.unwrap_or("/").trim_end_matches('/'));
                process::spawn_override_shell(&cmd, project_path)?
            }
            None => {
                let dev_script = read_dev_script(project_path)?;
                // preCmd 会改写 package.json/vite.config 并增加设计模式依赖，
                // 即使 node_modules 已存在也必须执行增量 install。对齐 nuwax
                // startDev_NonBlocking，安装失败必须阻止启动，不能带着缺包配置启动 Vite。
                let install_logs = LogFiles::new(&main_log, &temp_log);
                pnpm::install(
                    project_path,
                    &InstallOptions::prefer_offline(),
                    Some(&install_logs),
                    self.config.dev_command_timeout_secs,
                )
                .await
                .map_err(|error| {
                    AppError::system(format!("Dependency installation failed: {error}"))
                })?;
                let dev_args = process::build_dev_args(&dev_script, port, base_path)?;
                process::spawn_dev(
                    dev_args.program,
                    &dev_args.args,
                    project_path,
                    &dev_args.env_extra,
                )?
            }
        };
        let pid = child
            .id()
            .ok_or_else(|| AppError::system("spawned child has no pid"))?;
        // stderr 环形缓冲 (启动期保留末尾若干行, 供早退时结构化错误分类)
        let stderr_ring: Arc<StderrRing> = Arc::new(Mutex::new(
            std::collections::VecDeque::with_capacity(STDERR_RING_CAP),
        ));
        // 日志管道 (fire-and-forget): stdout 仅写日志; stderr 额外 tee 到 ring (借鉴 vite-rs 分流)
        if let Some(out) = stdout {
            log::spawn_log_pipe(out, main_log.clone(), temp_log.clone());
        }
        if let Some(err) = stderr {
            log::spawn_log_pipe_with_ring(
                err,
                main_log.clone(),
                temp_log.clone(),
                stderr_ring.clone(),
            );
        }
        // 丢弃 Child 句柄 (kill_on_drop=false → 进程独立存活, 靠 pid 杀)
        drop(child);

        // 就绪轮询 (在登记到 map 之前): 进程早退 → 读 stderr ring 分类成结构化错误;
        // 端口经 AllocGuard 自动释放。避免把"启动失败已死的 vite"当成成功。
        self.poll_alive(
            pid,
            port,
            base_path,
            &stderr_ring,
            &|port, base, timeout_ms| Box::pin(is_project_alive(port, base, timeout_ms)),
        )
        .await?;

        lock(&self.processes)?.insert(
            project_id.to_string(),
            DevProcess {
                pid,
                port,
                project_id: project_id.to_string(),
                started_at: now,
                log_dir: ldir.clone(),
                temp_log_name: log::temp_log_name(now),
            },
        );
        port_alloc.disarm(); // 分配成功且就绪, 不再归还端口 (Drop 变 no-op)

        Ok(StartedDev { pid, port })
    }

    /// 就绪轮询: 进程早退 → Err (读 stderr ring 分类成结构化错误); HTTP 就绪 → Ok;
    /// 超时但进程仍在 → Ok (nuwax 宽松)。
    async fn poll_alive(
        &self,
        pid: u32,
        port: u16,
        base_path: Option<&str>,
        stderr_ring: &Arc<StderrRing>,
        probe: AliveProbe<'_>,
    ) -> AppResult<()> {
        let max = self.config.dev_alive_max_wait_ms;
        let timeout = self.config.dev_alive_check_timeout_ms;
        let interval = Duration::from_millis(self.config.dev_alive_poll_interval_ms);
        // 用墙钟 deadline 计时 (而非固定步进累加): 探活本身可能耗时 (未 ready 时等到
        // timeout), 固定步进累加会让 max 超时判断严重偏松 (实际墙钟远大于累加值)。
        let deadline = std::time::Instant::now() + Duration::from_millis(max);
        loop {
            // 进程已退出 (端口冲突 / 配置错 / 依赖缺失) → 读 stderr 分类成可操作错误
            if !is_process_running(pid) {
                return Err(early_exit_err(pid, port, stderr_ring));
            }
            // deadline 检查放探活前: 兜底 max=0 边界 + sleep 后已超 deadline 不再多探活。
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            // 探活超时夹到剩余 deadline (min): 防单次探活越过 deadline, 让 max 成硬上限
            // (默认 max=30s >> timeout=1.5s 不触发, 仅防御 max<timeout 的错误配置)。
            // 首轮即探活 (不固定盲等 sleep): spawn 返回时 vite 端口未 listen, reqwest
            // connection refused 快速失败, 等价探测式等待; vite 提前 ready 能立刻发现。
            let this_timeout = timeout.min((deadline - now).as_millis() as u64);
            if probe(port, base_path, this_timeout).await {
                return Ok(());
            }
            tokio::time::sleep(interval).await;
        }
        // 超时: 进程仍存活但未响应 HTTP → 按 nuwax 宽松返回成功; 若已死则分类报错
        if !is_process_running(pid) {
            return Err(early_exit_err(pid, port, stderr_ring));
        }
        tracing::warn!(
            "dev server on port {port} (pid {pid}) 未在 {max}ms 内响应 HTTP, 进程仍在 — 返回成功 (nuwax 宽松)"
        );
        Ok(())
    }

    /// stop-dev (对齐 nuwax stopDevServerByProjectId; 系统级 pid 扫描 + 杀整组 +
    /// 释放端口 + 清 temp 日志)。候选 pid = 内存 Map pid ∪ `ps` 扫描 pid (去重)。
    pub async fn stop_dev(&self, project_id: &str) -> AppResult<StoppedDev> {
        let proc = lock(&self.processes)?.remove(project_id);
        // 候选 pid: 内存 Map + 系统扫描 (去重)
        let mut candidates: Vec<u32> = Vec::new();
        if let Some(p) = &proc {
            candidates.push(p.pid);
        }
        candidates.extend(process::find_pids_by_project_id(project_id).await);
        candidates.sort_unstable();
        candidates.dedup();

        let candidates: Vec<(u32, Option<u32>)> = candidates
            .into_iter()
            .map(|pid| (pid, process::process_group_id(pid)))
            .collect();
        let mut stopped_groups = HashSet::new();
        let mut killed: Vec<KilledPid> = Vec::new();
        for (pid, pgid) in candidates {
            // 第一个成员已通过 kill(-pgid) 停止整组，其余成员不应
            // 因无法再次发送信号而被误报为 false。
            if pgid.is_some_and(|group| stopped_groups.contains(&group)) {
                killed.push(KilledPid { pid, killed: true });
                continue;
            }
            let ok = kill_process_group(pid);
            process::wait_for_stop(
                pid,
                self.config.dev_stop_check_interval_ms,
                self.config.dev_stop_max_attempts,
            )
            .await;
            let mut k = ok;
            if is_process_running(pid) {
                tracing::warn!("dev server (pid {pid}) 未在 SIGTERM 宽限期退出, 升级 SIGKILL");
                let force_sent = process::kill_process_group_force(pid);
                process::wait_for_stop(
                    pid,
                    self.config.dev_stop_check_interval_ms,
                    self.config.dev_stop_max_attempts,
                )
                .await;
                // zombie 进程在父进程回收前 `kill(pid, 0)` 仍会返回存在，
                // 但 SIGKILL 已成功送达时业务上应视为 killed，对齐 nuwax killProcess。
                k = k || force_sent || !is_process_running(pid);
            }
            if k && let Some(group) = pgid {
                stopped_groups.insert(group);
            }
            killed.push(KilledPid { pid, killed: k });
        }
        if let Some(p) = proc {
            self.port_pool.release(project_id);
            log::cleanup_temp_logs(&p.log_dir).await;
        }
        Ok(StoppedDev {
            killed_pids: killed,
        })
    }

    /// restart-dev = stop + start。
    pub async fn restart_dev(
        &self,
        project_id: &str,
        project_path: &Path,
        base_path: Option<&str>,
    ) -> AppResult<StartedDev> {
        self.stop_dev(project_id).await?;
        self.start_dev(project_id, project_path, base_path).await
    }

    /// keep-alive (对齐 nuwax: 探活, 不存活则重启)。
    pub async fn keep_alive(
        &self,
        project_id: &str,
        _pid: u32,
        port: u16,
        base_path: Option<&str>,
        project_path: &Path,
    ) -> AppResult<KeepAliveResult> {
        let alive = is_project_alive(port, base_path, self.config.dev_alive_check_timeout_ms).await;
        if alive {
            return Ok(KeepAliveResult {
                alive: true,
                action: None,
                pid: None,
                port: None,
            });
        }
        // 不存活 → 重启, 返回新 pid/port (对齐 nuwax 透传 startDevServer 返回值)
        self.stop_dev(project_id).await?;
        let started = self.start_dev(project_id, project_path, base_path).await?;
        Ok(KeepAliveResult {
            alive: true,
            action: Some("start".into()),
            pid: Some(started.pid),
            port: Some(started.port),
        })
    }

    /// list-dev: 内存 Map 快照。
    pub fn list_dev(&self) -> AppResult<Vec<DevProcess>> {
        Ok(lock(&self.processes)?.values().cloned().collect())
    }

    /// port-pool-status。
    pub fn port_pool_status(&self) -> AppResult<PortPoolStatus> {
        self.port_pool.status()
    }

    /// get-dev-log。
    pub async fn read_dev_log(
        &self,
        project_id: &str,
        start_index: usize,
        log_type: &str,
    ) -> AppResult<ReadDevLogResult> {
        let dir = log::log_dir(&self.config, project_id);
        read_dev_log(&dir, start_index, log_type, self.config.log_read_max_bytes).await
    }

    async fn write_npmrc(&self, project_path: &Path) -> AppResult<()> {
        // create .npmrc 模板 + sanitize built-deps 冲突 (对齐 exec.rs 的清理逻辑)。
        // 不调 ensure_pnpm_install_config: 其 append 会加 dangerously-allow-all-builds=true,
        // 在 pnpm 10.x 下与内置 neverBuiltDependencies 冲突 (ERR_PNPM_CONFIG_CONFLICT_BUILT_DEPENDENCIES),
        // 而 vite dev 的依赖 (esbuild) 走可选依赖机制、不需 build 脚本。NO_TTY 由 pnpm cli 的
        // --config.confirmModulesPurge=false 兜底 (见 pnpm/cli.rs)。
        crate::service::pnpm_config::create_pnpm_npmrc(project_path).await?;
        crate::service::pnpm_config::sanitize_pnpm_built_dependencies_config(project_path).await
    }

    /// 全量优雅停止 (供 main.rs graceful shutdown 调用):
    /// 逐个项目走完整 `stop_dev` 流程 (SIGTERM → 等 → SIGKILL + ps 扫描 + 还端口 + 清日志)。
    /// 幂等: 进程已不在也安全返回; 单项失败记 warn 不中断其余。
    pub async fn shutdown_all(&self) {
        let snapshot: Vec<String> = lock(&self.processes)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        if snapshot.is_empty() {
            return;
        }
        tracing::info!("shutdown_all: stopping {} dev server(s)", snapshot.len());
        for project_id in snapshot {
            if let Err(e) = self.stop_dev(&project_id).await {
                tracing::warn!(%project_id, "shutdown_all stop failed: {e}");
            }
        }
    }
}

/// Drop 兜底: 正常路径 (graceful shutdown) 已由 `shutdown_all` 清空实例表;
/// 此处只防 "panic / Arc 提前释放 / shutdown_all 未触发" 残留。
///
/// 约束: `Drop::drop` 不能 `.await`, 无法给 SIGTERM grace 宽限期 ——
/// 发 SIGTERM 后无等待地 SIGKILL 等价于直接 SIGKILL, 故此处省去无意义的 SIGTERM,
/// 直接对进程组 SIGKILL, 确保进程终止并还端口、清表。
///
/// ⚠️ 不覆盖场景: file-server 自身被 SIGKILL 强杀时进程直接终止, **Drop 不会执行**,
/// detached 的 dev server 仍会成孤儿 —— 这是 detached 模型的固有局限, 只能靠
/// 容器/编排层 (Pod 退出回收) 兜底。正常重启走 SIGTERM → `shutdown_all` 路径已覆盖。
impl Drop for DevServerManager {
    fn drop(&mut self) {
        let Ok(procs) = self.processes.get_mut() else {
            return;
        };
        if procs.is_empty() {
            return;
        }
        tracing::warn!(
            "DevServerManager dropped with {} live dev server(s) — best-effort SIGKILL",
            procs.len()
        );
        for (project_id, p) in procs.iter() {
            // 兜底硬杀: SIGKILL 进程组 (无法 await, 故不走 SIGTERM→等→SIGKILL 升级)
            if !process::kill_process_group_force(p.pid) {
                tracing::warn!(%project_id, pid = p.pid, "SIGKILL failed in Drop");
            }
            self.port_pool.release(project_id);
        }
        procs.clear();
    }
}

/// 端口分配守卫: drop 时归还 (仅 start 失败路径生效; 成功路径调 `disarm()` 使 Drop 变 no-op)。
struct AllocGuard<'a> {
    pool: &'a PortPool,
    project_id: String,
    armed: bool,
}
impl AllocGuard<'_> {
    /// 解除武装: 分配成功且就绪后调用, 使后续 Drop 不再归还端口 (取代 `mem::forget`)。
    fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for AllocGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.pool.release(&self.project_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_args_vite_with_base() {
        let d = build_args_test("vite", 4001, Some("foo")).unwrap();
        assert_eq!(d.program, "npx");
        assert!(d.args.iter().any(|a| a == "vite"));
        assert!(d.args.iter().any(|a| a == "4001"));
        assert!(d.args.iter().any(|a| a == "--strictPort"));
        assert!(d.args.iter().any(|a| a == "0.0.0.0"));
        // --clearScreen false 抑制 ANSI 清屏转义污染日志
        assert!(d.args.iter().any(|a| a == "--clearScreen"));
        assert!(
            d.args
                .windows(2)
                .any(|w| w[0] == "--clearScreen" && w[1] == "false")
        );
        assert!(d.args.iter().any(|a| a == "--base"));
        assert!(d.args.iter().any(|a| a == "/foo/"));
        assert!(d.env_extra.is_empty());
    }

    #[test]
    fn build_args_next_sets_base_env() {
        let d = build_args_test("next dev", 4002, Some("bar")).unwrap();
        assert_eq!(d.program, "npx");
        assert!(d.args.iter().any(|a| a == "next"));
        assert!(d.args.iter().any(|a| a == "4002"));
        assert!(
            d.env_extra
                .iter()
                .any(|(k, v)| k == "BASE_PATH" && v == "/bar/")
        );
    }

    #[test]
    fn build_args_rejects_unsupported() {
        assert!(build_args_test("webpack serve", 4003, None).is_err());
    }

    #[test]
    fn build_args_default_base_is_full_proxy_path() {
        // base 为空时默认 /proxy/{port}/ (HMR 依赖: base 必须是完整代理路径)
        let d = build_args_test("vite", 4005, None).unwrap();
        assert!(d.args.iter().any(|a| a == "--base"));
        assert!(d.args.iter().any(|a| a == "/proxy/4005/"));
    }

    fn build_args_test(script: &str, port: u16, base: Option<&str>) -> AppResult<process::DevArgs> {
        process::build_dev_args(script, port, base)
    }

    #[tokio::test]
    async fn shutdown_all_empty_is_noop() {
        // 空实例表 → 立即返回, 不 panic
        let mgr = DevServerManager::new(Arc::new(Config::from_env().expect("test config")));
        mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn drop_with_stale_entry_does_not_panic() {
        // 塞一个不可能存活的 pid: Drop 对其 SIGKILL 返回 false 但不 panic, 仍还端口 + 清表
        let mgr = DevServerManager::new(Arc::new(Config::from_env().expect("test config")));
        {
            let mut procs = mgr.processes.lock().unwrap();
            procs.insert(
                "ghost".to_string(),
                DevProcess {
                    pid: 999_999,
                    port: 0,
                    project_id: "ghost".to_string(),
                    started_at: 0,
                    log_dir: PathBuf::from("/tmp/nonexistent-fs-test"),
                    temp_log_name: "ghost.log".to_string(),
                },
            );
        }
        drop(mgr); // 不 panic 即通过
    }

    #[tokio::test]
    async fn poll_alive_returns_promptly_when_server_ready() {
        // 探活用即时返回 true 的 stub：绕开 reqwest Client 构建 / 系统 CA 加载的固有延迟
        // （macOS keychain 首加载数百 ms，会让真实探活的计时断言 flaky）。焦点纯粹在 poll_alive
        // 的循环逻辑——server ready 时首轮探活即返回，而非固定盲等 sleep。
        let mut config = Config::from_env().expect("test config");
        config.dev_alive_poll_interval_ms = 10;
        let mgr = DevServerManager::new(Arc::new(config));
        // pid 用当前进程 → is_process_running 恒 true
        let pid = std::process::id();
        let ring: Arc<StderrRing> = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let start = std::time::Instant::now();
        let res = mgr
            .poll_alive(
                pid,
                0,
                None,
                &ring,
                &|_port, _base, _timeout| Box::pin(async { true }),
            )
            .await;
        let elapsed = start.elapsed();
        assert!(res.is_ok(), "ready 时 poll_alive 应成功");
        // 即时探活 + 10ms 间隔，首轮即返回，应远 < 200ms（原固定 1s sleep > 1s）。
        assert!(
            elapsed.as_millis() < 200,
            "poll_alive 耗时 {elapsed:?}, 期望 < 200ms (即时探活 stub, 首轮即返回)"
        );
    }
}

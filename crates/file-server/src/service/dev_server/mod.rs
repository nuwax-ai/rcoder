//! Dev server 进程管理器 (对齐 nuwax `processManager.js` + `startDevUtils.js` 等)。
//!
//! 替换 nuwax 的裸 `child_process.spawn` (detached) + 内存 Map。
//! 用 `tokio::process::Command` (process_group + 丢弃 Child 句柄) + `Mutex<HashMap>`。
//!
//! 注意 (对齐 nuwax 已知宽松行为):
//! - 纯内存状态, 无持久化 (重启即丢)
//! - 存活轮询超时后仍返回成功 (nuwax 行为)
//! - keep-alive 被动探活, 无空闲自动停止

pub mod log;
pub mod port_pool;
pub mod process;

pub use log::{read_dev_log, ReadDevLogResult};
pub use port_pool::{PortAllocation, PortPool, PortPoolStatus};
pub use process::{is_process_running, is_project_alive, kill_process_group, now_ms};

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::{AppError, AppResult};
use crate::Config;

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
        self.start_dev_inner(project_id, project_path, base_path).await
    }

    async fn start_dev_inner(
        &self,
        project_id: &str,
        project_path: &Path,
        base_path: Option<&str>,
    ) -> AppResult<StartedDev> {
        // 幂等: 已运行则返回现有 pid/port
        if let Some(p) = lock(&self.processes)?.get(project_id).cloned() {
            return Ok(StartedDev { pid: p.pid, port: p.port });
        }

        let port = self.port_pool.allocate(project_id)?;
        let port_alloc = AllocGuard { pool: &self.port_pool, project_id: project_id.to_string() };

        let ldir = log::log_dir(&self.config, project_id);
        tokio::fs::create_dir_all(&ldir)
            .await
            .map_err(|e| AppError::system(format!("create dev log dir: {e}")))?;
        let now = now_ms();
        let main_log = ldir.join(log::main_log_name());
        let temp_log = ldrtemp(&ldir, now);

        // 命令: override env (运维控制, 非用户输入 → sh -c 安全) 优先;
        // 否则从 package.json 读 dev script → arg 数组 (用户输入经此路径, 避免注入)
        let ovr = std::env::var("DEV_SERVER_OVERRIDE_CMD")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let (child, stdout, stderr) = match ovr {
            Some(ovr) => {
                let cmd = ovr
                    .replace("{PORT}", &port.to_string())
                    .replace("{BASE}", base_path.unwrap_or("/").trim_end_matches('/'));
                process::spawn_override_shell(&cmd, project_path)?
            }
            None => {
                let dev_script = read_dev_script(project_path)?;
                self.write_npmrc(project_path).await?;
                // node_modules 缺失则 install (失败不阻塞, 与 nuwax 宽松一致)
                if !project_path.join("node_modules").exists() {
                    let _ = process::run_command_to_log(
                        "pnpm",
                        &["install", "--prefer-offline"],
                        project_path,
                        &main_log,
                        &temp_log,
                        self.config.dev_command_timeout_secs,
                    )
                    .await;
                }
                let dev_args = process::build_dev_args(&dev_script, port, base_path)?;
                process::spawn_dev(dev_args.program, &dev_args.args, project_path, &dev_args.env_extra)?
            }
        };
        let pid = child
            .id()
            .ok_or_else(|| AppError::system("spawned child has no pid"))?;
        // 日志管道 (fire-and-forget)
        if let Some(out) = stdout {
            log::spawn_log_pipe(out, main_log.clone(), temp_log.clone());
        }
        if let Some(err) = stderr {
            log::spawn_log_pipe(err, main_log.clone(), temp_log.clone());
        }
        // 丢弃 Child 句柄 (kill_on_drop=false → 进程独立存活, 靠 pid 杀)
        drop(child);

        // 就绪轮询 (在登记到 map 之前): 进程早退 → 立即判失败 (端口经 AllocGuard 自动释放),
        // 避免把"启动失败已死的 vite"当成成功 (vite-rs 靠 sleep 2s 掩盖了这个 bug)
        self.poll_alive(pid, port, base_path).await?;

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
        std::mem::forget(port_alloc); // 分配成功且就绪, 不释放

        Ok(StartedDev { pid, port })
    }

    /// 就绪轮询: 进程早退 → Err; HTTP 就绪 → Ok; 超时但进程仍在 → Ok (nuwax 宽松)。
    async fn poll_alive(&self, pid: u32, port: u16, base_path: Option<&str>) -> AppResult<()> {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let max = self.config.dev_alive_max_wait_ms;
        let timeout = self.config.dev_alive_check_timeout_ms;
        let mut elapsed = 1000u64;
        while elapsed < max {
            // 进程已退出 (端口冲突 / 配置错 / 依赖缺失) → 立即失败
            if !is_process_running(pid) {
                return Err(AppError::system(format!(
                    "dev server (pid {pid}) exited during startup — 检查 dev 日志 (端口冲突/配置错误/依赖缺失)"
                )));
            }
            if is_project_alive(port, base_path, timeout).await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            elapsed += 1000;
        }
        // 超时: 进程仍存活但未响应 HTTP → 按 nuwax 宽松返回成功; 若已死则报错
        if !is_process_running(pid) {
            return Err(AppError::system(format!(
                "dev server (pid {pid}) exited during startup"
            )));
        }
        tracing::warn!(
            "dev server on port {port} (pid {pid}) 未在 {max}ms 内响应 HTTP, 进程仍在 — 返回成功 (nuwax 宽松)"
        );
        Ok(())
    }

    /// stop-dev (对齐 nuwax stopDevServerByProjectId; 杀整组 + 释放端口 + 清 temp 日志)。
    pub async fn stop_dev(&self, project_id: &str) -> AppResult<StoppedDev> {
        let proc = lock(&self.processes)?.remove(project_id);
        let killed = match proc {
            Some(p) => {
                let pid = p.pid;
                let ok = kill_process_group(pid);
                // SIGTERM 宽限期 (默认 5s); 仍存活则 SIGKILL 强杀, 避免残留
                process::wait_for_stop(
                    pid,
                    self.config.dev_stop_check_interval_ms,
                    self.config.dev_stop_max_attempts,
                )
                .await;
                let mut killed = ok;
                if is_process_running(pid) {
                    tracing::warn!(
                        "dev server (pid {pid}) 未在 SIGTERM 宽限期退出, 升级 SIGKILL"
                    );
                    process::kill_process_group_force(pid);
                    process::wait_for_stop(
                        pid,
                        self.config.dev_stop_check_interval_ms,
                        self.config.dev_stop_max_attempts,
                    )
                    .await;
                    killed = !is_process_running(pid);
                }
                self.port_pool.release(project_id);
                log::cleanup_temp_logs(&p.log_dir).await;
                vec![KilledPid { pid, killed }]
            }
            None => vec![],
        };
        Ok(StoppedDev { killed_pids: killed })
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
        let alive =
            is_project_alive(port, base_path, self.config.dev_alive_check_timeout_ms).await;
        if alive {
            return Ok(KeepAliveResult { alive: true, action: None });
        }
        // 不存活 → 重启
        self.stop_dev(project_id).await?;
        self.start_dev(project_id, project_path, base_path).await?;
        Ok(KeepAliveResult {
            alive: true,
            action: Some("start".into()),
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
        read_dev_log(&dir, start_index, log_type).await
    }

    async fn write_npmrc(&self, project_path: &Path) -> AppResult<()> {
        let npmrc = project_path.join(".npmrc");
        // JuiceFS 上 hardlink 会失败, 强制 copy
        tokio::fs::write(npmrc, "package-import-method=copy\n")
            .await
            .map_err(|e| AppError::system(format!("write .npmrc: {e}")))?;
        Ok(())
    }
}

/// 端口分配守卫: drop 时归还 (仅 start 失败时生效; 成功时 forget)。
struct AllocGuard<'a> {
    pool: &'a PortPool,
    project_id: String,
}
impl Drop for AllocGuard<'_> {
    fn drop(&mut self) {
        self.pool.release(&self.project_id);
    }
}

fn ldrtemp(ldir: &Path, now: i64) -> PathBuf {
    ldir.join(log::temp_log_name(now))
}

/// 读 package.json 的 scripts.dev (对齐 nuwax startDevUtils)。
fn read_dev_script(project_path: &Path) -> AppResult<String> {
    let pkg_path = project_path.join("package.json");
    let content = std::fs::read_to_string(&pkg_path)
        .map_err(|e| AppError::business(format!("read package.json failed: {e}")))?;
    let pkg: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| AppError::business(format!("parse package.json failed: {e}")))?;
    let dev = pkg
        .get("scripts")
        .and_then(|s| s.get("dev"))
        .and_then(|d| d.as_str())
        .ok_or_else(|| AppError::business("package.json has no scripts.dev"))?;
    Ok(dev.to_string())
}

fn lock<T>(m: &Mutex<T>) -> AppResult<std::sync::MutexGuard<'_, T>> {
    m.lock().map_err(|e| AppError::system(format!("mutex poisoned: {e}")))
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
        assert!(d.args.windows(2).any(|w| w[0] == "--clearScreen" && w[1] == "false"));
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
        assert!(d
            .env_extra
            .iter()
            .any(|(k, v)| k == "BASE_PATH" && v == "/bar/"));
    }

    #[test]
    fn build_args_rejects_unsupported() {
        assert!(build_args_test("webpack serve", 4003, None).is_err());
    }

    fn build_args_test(
        script: &str,
        port: u16,
        base: Option<&str>,
    ) -> AppResult<process::DevArgs> {
        process::build_dev_args(script, port, base)
    }
}

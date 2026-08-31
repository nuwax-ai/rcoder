//! dev server 启动: start_dev / start_dev_inner / poll_alive + 端口/启动守卫。

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::error_classify::{STDERR_RING_CAP, StderrRing};
use super::log;
use super::port_pool::PortPool;
use super::process;
use super::support::{early_exit_err, ldrtemp, lock, read_dev_script};
use super::types::{AliveProbe, DevServerManager, StartedDev};
use crate::error::{AppError, AppResult};
use crate::models::DevProcess;
use crate::service::pnpm::{self, InstallOptions, LogFiles};
use std::path::Path;

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

impl DevServerManager {
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

    pub(super) async fn start_dev_inner(
        &self,
        project_id: &str,
        project_path: &Path,
        base_path: Option<&str>,
    ) -> AppResult<StartedDev> {
        // UserApp workspace 分流：workspace.manifest.toml 存在 → app-cli 引擎。
        // manifest 多服务（Java/Go 等）的正确运行态 = app-cli 按 run.command
        // 编排全栈 + pingap 9080 统一入口（[proxy] 路由）；开发容器 per-app，
        // 9080 无冲突。原 package.json/vite 引擎是 web 域（单 vite dev server）
        // 移植，对多服务模板不适用——web/computer 项目（无 workspace manifest）
        // 继续走原路径。
        if project_path.join("workspace.manifest.toml").exists() {
            return self.start_dev_manifest(project_id, project_path).await;
        }
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
        let now = process::now_ms();
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
            Default::default(),
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
            &|port, base, timeout_ms| Box::pin(process::is_project_alive(port, base, timeout_ms)),
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

    /// UserApp workspace 的 dev 启动（app-cli 引擎）：spawn 常驻 `app-cli
    /// --workspace <ws>` ——按 manifest run.command 拉起全部服务 + pingap
    /// 9080 统一入口（多服务编排/健康检查/失败清理都由 app-cli 负责）。
    ///
    /// 端口恒 9080（pingap 主入口，per-app 开发容器无冲突），不走 PortPool；
    /// 探活沿用 poll_alive（app-cli 早退=manifest 校验失败被拦截；HTTP 未
    /// 就绪但进程存活=宽松通过，与 vite 路径同语义）。app-cli 自身文件日志
    /// 指 `<log_dir>/app-cli/`，stdout/stderr 管道照走 main_log；编排日志的
    /// 对外查询口=logs/query 内置源（service_id=app-cli / source_id=orchestrator）。
    async fn start_dev_manifest(
        &self,
        project_id: &str,
        project_path: &Path,
    ) -> AppResult<StartedDev> {
        // 单一来源 shared_types::APP_ENTRY_PORT（release 流程、Pingora 免端口代理同值）
        const PINGAP_ENTRY_PORT: u16 = shared_types::APP_ENTRY_PORT;
        // 幂等: 已运行则返回现有 pid/port（app-cli 路径与 vite 路径同表登记）
        if let Some(p) = lock(&self.processes)?.get(project_id).cloned() {
            return Ok(StartedDev {
                pid: p.pid,
                port: p.port,
            });
        }

        let ldir = log::log_dir(&self.config, project_id);
        tokio::fs::create_dir_all(ldir.join("app-cli"))
            .await
            .map_err(|e| AppError::system(format!("create app-cli log dir: {e}")))?;
        let now = process::now_ms();
        let main_log = ldir.join(log::main_log_name());
        let temp_log = ldrtemp(&ldir, now);

        // 管理 API 绑随机端口（dev 场景无人消费，避免多实例撞 3010）
        let (child, stdout, stderr) = process::spawn_dev(
            "app-cli",
            &[
                "--workspace".to_string(),
                project_path.display().to_string(),
                "--log-dir".to_string(),
                ldir.join("app-cli").display().to_string(),
                "--admin-addr".to_string(),
                "127.0.0.1:0".to_string(),
            ],
            project_path,
            &[],
        )?;
        let pid = child
            .id()
            .ok_or_else(|| AppError::system("spawned app-cli has no pid"))?;
        let stderr_ring: Arc<StderrRing> = Arc::new(Mutex::new(
            std::collections::VecDeque::with_capacity(STDERR_RING_CAP),
        ));
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
        drop(child);

        // 早退检测 + 宽松就绪（pingap 按 [proxy] path 路由，根路径可能 404——
        // HTTP 判不通但进程存活即通过）
        self.poll_alive(
            pid,
            PINGAP_ENTRY_PORT,
            None,
            &stderr_ring,
            &|port, _base, timeout_ms| {
                Box::pin(process::is_project_alive(port, Some("/"), timeout_ms))
            },
        )
        .await?;

        lock(&self.processes)?.insert(
            project_id.to_string(),
            DevProcess {
                pid,
                port: PINGAP_ENTRY_PORT,
                project_id: project_id.to_string(),
                started_at: now,
                log_dir: ldir.clone(),
                temp_log_name: log::temp_log_name(now),
            },
        );

        Ok(StartedDev {
            pid,
            port: PINGAP_ENTRY_PORT,
        })
    }

    /// 就绪轮询: 进程早退 → Err (读 stderr ring 分类成结构化错误); HTTP 就绪 → Ok;
    /// 超时但进程仍在 → Ok (nuwax 宽松)。
    pub(super) async fn poll_alive(
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
            if !process::is_process_running(pid) {
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
        if !process::is_process_running(pid) {
            return Err(early_exit_err(pid, port, stderr_ring));
        }
        tracing::warn!(
            "dev server on port {port} (pid {pid}) 未在 {max}ms 内响应 HTTP, 进程仍在 — 返回成功 (nuwax 宽松)"
        );
        Ok(())
    }

    pub(super) async fn write_npmrc(&self, project_path: &Path) -> AppResult<()> {
        // create .npmrc 模板 + sanitize built-deps 冲突 (对齐 exec.rs 的清理逻辑)。
        // 不调 ensure_pnpm_install_config: 其 append 会加 dangerously-allow-all-builds=true,
        // 在 pnpm 10.x 下与内置 neverBuiltDependencies 冲突 (ERR_PNPM_CONFIG_CONFLICT_BUILT_DEPENDENCIES),
        // 而 vite dev 的依赖 (esbuild) 走可选依赖机制、不需 build 脚本。NO_TTY 由 pnpm cli 的
        // --config.confirmModulesPurge=false 兜底 (见 pnpm/cli.rs)。
        crate::service::pnpm_config::create_pnpm_npmrc(project_path).await?;
        crate::service::pnpm_config::sanitize_pnpm_built_dependencies_config(project_path).await
    }
}

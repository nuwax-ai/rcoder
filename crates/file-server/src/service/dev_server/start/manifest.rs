use super::*;
use crate::service::dev_server::discovery;

impl DevServerManager {
    /// Userapp workspace 的 dev 启动（app-cli 引擎）：spawn 常驻 `app-cli run
    /// --workspace <ws>` ——按 manifest run.command 拉起全部服务 + pingap
    /// 9080 统一入口（多服务编排/健康检查/失败清理都由 app-cli 负责）。
    ///
    /// 端口恒 9080（pingap 主入口，per-app 开发容器无冲突），不走 PortPool；
    /// 探活沿用 poll_alive（app-cli 早退=manifest 校验失败被拦截；HTTP 未
    /// 就绪但进程存活=宽松通过，与 vite 路径同语义）。app-cli 自身文件日志
    /// 指 `<log_dir>/app-cli/`，stdout/stderr 管道照走 main_log；编排日志的
    /// 对外查询口=logs/query 内置源（service_id=app-cli / source_id=orchestrator）。
    pub(super) async fn start_dev_manifest(
        &self,
        project_id: &str,
        project_path: &Path,
        launch: super::super::DevLaunch<'_>,
    ) -> AppResult<StartedDev> {
        let (hooks, pg, request_context, artifact_release_id) = (
            launch.hooks.clone(),
            launch.pg,
            launch.request_context,
            launch.artifact_release_id,
        );
        // 单一来源 shared_types::APP_ENTRY_PORT（release 流程、Pingora 免端口代理同值）
        const PINGAP_ENTRY_PORT: u16 = shared_types::APP_ENTRY_PORT;
        if let Some(process) = lock(&self.processes)?.get(project_id).cloned()
            && process.external_owner.is_none()
        {
            return Err(AppError::business(
                "local orchestrator is already registered; readiness must be confirmed or an explicit restart requested",
            ));
        }
        // P1-05：旧 supervised 停止后未确认清理（如进程组残留或 stdout 排空未完成）——
        // 拒绝新 manifest 启动，直到后台清理确认完成。避免并发 dev 操作撞端口或误用残留状态。
        if self.has_uncleaned_cleanup(project_id)? {
            return Err(AppError::business(
                "previous orchestrator cleanup unconfirmed; retry after stop completes",
            ));
        }

        // P3-02：owner 感知——spawn 前探测 3010。匹配本 workspace 的 serve
        // owner 经运行 API 复用（消除平台/agent 双启动的 3010 冲突）；legacy
        // app-cli / foreign 应答明确拒绝（XP04：不杀对方、不盲 spawn）。
        // DEV-R1：serve 复用先于磁盘 phase 门禁——监督 phase=Ready 的 serve
        // 不能被"非 Stopped 就拒绝"挡在复用之前。DEV-R6：复用提交携带本次
        // 制品身份（artifact_release_id）——制品态不得退化为 Source Restart。
        if let Some(started) = self
            .reuse_or_refuse_owner(
                project_id,
                project_path,
                hooks.clone(),
                pg,
                artifact_release_id,
                request_context,
            )
            .await?
        {
            return Ok(started);
        }
        // DEV-R1：serve 未复用后分类本地 run 目标——磁盘 phase 单独不是门禁：
        // 活 run 拒绝（短预算探测，非 Stopped 即活）；死记录（进程已退、
        // owner.lock 可取）就地离线收束后放行，用户无需删除状态文件。
        self.ensure_no_local_execution(project_id, project_path)
            .await?;

        let ldir = log::log_dir(&self.config, project_id);
        tokio::fs::create_dir_all(ldir.join("app-cli"))
            .await
            .map_err(|e| AppError::system(format!("create app-cli log dir: {e}")))?;
        let now = process::now_ms();
        let main_log = ldir.join(log::main_log_name());
        let temp_log = ldrtemp(&ldir, now);

        // 管理 API 绑 0.0.0.0:3010——logs/query 的 orchestrator 内置源
        //（app_manager log_api_base dev 分支）按 {容器 IP}:3010 连 app-cli
        // 日志 API，随机端口/loopback 都会断链（旧"避免多实例撞 3010"顾虑
        // 基于多 app 同沙箱的已废弃架构；per-app 容器单实例无冲突）。
        // env 注入 APP_CLI_RUN_PROFILE=dev：源码态 dev 链路信号——app-cli 编排
        // 时 [devrun].command 优先、[run].command 兜底（产物态/生产不注入恒走
        // [run]，见 app-cli supervisor::effective_run_argv）。
        // pg 凭据（可选，与 prod StartAppRequest.pg 同构 wire）：注入
        // POSTGRES_USER/POSTGRES_PASSWORD 覆盖容器 env 透传值（minimal_env
        // extra last-wins）——save-db-credential 改密后 dev start/restart 由
        // 调用方携带新凭据，编排的服务进程才能拿到与容器内 PG 一致的密码。
        // P1-03：编排器程序可经 config.app_cli_bin 覆盖（默认 PATH 的 app-cli；
        // 测试注入受控假编排器，生产行为不变）。
        let program = self.config.app_cli_bin.as_deref().unwrap_or("app-cli");
        let mut env_extra = vec![("APP_CLI_RUN_PROFILE".to_string(), "dev".to_string())];
        if let Some(pg) = pg {
            env_extra.push(("POSTGRES_USER".to_string(), pg.username.clone()));
            env_extra.push(("POSTGRES_PASSWORD".to_string(), pg.password.clone()));
        }
        // DEV-1 Fix A：平台身份/状态根透传——子编排器与父进程（file-server）
        // 使用同一根与项目身份，消除 spawn 环境过滤造成的 standalone registry
        // 段与平台根分岔（nuwax-k8s-test app 211 事故根因：stop 解析到平台根
        // 的旧实例残留，活编排器在 registry 段根上毫发无损）。仅当平台身份
        // 指向本项目时透传；桌面 standalone（无平台变量）保持原状。
        let platform_state_root = platform_launch_env(
            std::env::var_os("PROJECT_ID").as_deref(),
            std::env::var_os("APP_CLI_STATE_ROOT").as_deref(),
            project_path,
            &mut env_extra,
        );
        if let Some(root) = &platform_state_root {
            append_managed_launch_env(project_path, root, &mut env_extra, |key| {
                std::env::var_os(key)
            })
            .map_err(|error| {
                AppError::business(format!("managed launch context rejected: {error:#}"))
            })?;
        }
        // 迁移回执位置绑定（DEV-R5）：由 apply_migration_receipts_binding 证据
        // 决策（显式根/旧目录两侧的 pending/completed + 持久位置记录）后注入，
        // 不再按"旧目录存在"猜测——pending 不被路径切换掩盖、已完成不重跑。
        self.apply_migration_receipts_binding(
            project_id,
            project_path,
            platform_state_root.as_deref(),
            &mut env_extra,
        )?;
        let (child, stdout, stderr) = process::spawn_dev(
            program,
            &[
                "run".to_string(),
                "--workspace".to_string(),
                project_path.display().to_string(),
                "--log-dir".to_string(),
                ldir.join("app-cli").display().to_string(),
                "--admin-addr".to_string(),
                "0.0.0.0:3010".to_string(),
            ],
            project_path,
            &env_extra,
        )?;
        // P1-03：Child 收编唯一监督 worker（wait/reap 真实 ExitStatus + stdout
        // 管道句柄 + stderr ring）——不再 drop(child)；停止仍走进程组信号路径。
        let stderr_ring: Arc<StderrRing> = Arc::new(Mutex::new(
            std::collections::VecDeque::with_capacity(STDERR_RING_CAP),
        ));
        let supervised = SupervisedChild::adopt(child, stderr_ring.clone());
        let pid = supervised.pid();
        if let Some(out) = stdout {
            // EVT 识别（hooks Some 时回调编排事件行；None 退化为 no-op 闭包，
            // 日志行为不变）；管道结束（EOF/读错）经 hooks.on_end 上报恰好一次。
            let pipe_hooks =
                hooks.unwrap_or_else(crate::service::dev_server::supervise::DevEventHooks::noop);
            let pipe = log::spawn_log_pipe_with_events(
                out,
                main_log.clone(),
                temp_log.clone(),
                pipe_hooks,
            );
            supervised.attach_stdout(pipe);
        }
        if let Some(err) = stderr {
            log::spawn_log_pipe_with_ring(
                err,
                main_log.clone(),
                temp_log.clone(),
                stderr_ring.clone(),
            );
        }

        // DEV-1 §3.1 / 复核 DEV-R2：spawn 已发生、探活未过的窗口先发表可捕获
        // 的 Starting 记录——launch 身份 + 进程 + **监督句柄**三张表在单一
        // 同步临界区（固定锁序 launches→processes→supervised，不持锁跨
        // await）原子发表；并发 Stop 不得在此窗口看到"有 launch 无
        // supervised"的半登记。失败路径按同一身份自清，不补登记成运行成功。
        let launch_id = uuid::Uuid::now_v7().simple().to_string();
        {
            let mut launches = lock(&self.launches)?;
            let mut processes = lock(&self.processes)?;
            let mut supervised_map = lock(&self.supervised)?;
            processes.insert(
                project_id.to_string(),
                DevProcess {
                    pid,
                    port: PINGAP_ENTRY_PORT,
                    project_id: project_id.to_string(),
                    instance_id: None,
                    base_path: None,
                    started_at: now,
                    log_dir: ldir.clone(),
                    temp_log_name: log::temp_log_name(now),
                    external_owner: None,
                },
            );
            launches.insert(
                project_id.to_string(),
                super::super::types::LocalLaunch {
                    launch_id: launch_id.clone(),
                    state_root: platform_state_root,
                },
            );
            supervised_map.insert(project_id.to_string(), supervised.clone());
        }

        // 早退检测 + 宽松就绪（pingap 按 [proxy] path 路由，根路径可能 404——
        // HTTP 判不通但进程存活即通过）
        if let Err(error) = self
            .poll_alive(
                pid,
                PINGAP_ENTRY_PORT,
                None,
                &stderr_ring,
                &|port, _base, timeout_ms| {
                    Box::pin(process::is_project_alive(port, Some("/"), timeout_ms))
                },
            )
            .await
        {
            // §3.1：探活失败清理自己刚创建的精确进程树与 Starting 登记
            //（启动互斥锁在手，不存在更新的 launch 可被误伤）。
            let _ = self.terminate_pid_group(pid).await;
            {
                let mut launches = lock(&self.launches)?;
                let mut processes = lock(&self.processes)?;
                let mut supervised_map = lock(&self.supervised)?;
                launches.remove(project_id);
                processes.remove(project_id);
                supervised_map.remove(project_id);
            }
            return Err(error);
        }

        Ok(StartedDev {
            pid,
            port: PINGAP_ENTRY_PORT,
        })
    }

    /// DEV-R1：分类并清场本地执行目标（Start 在激活运行目录之前调用，
    /// 保证"拒绝前不改运行目录"——复核 DEV-R6）。磁盘 phase 单独不是门禁：
    /// - 活 run（短预算 Status 探测有应答）→ 拒绝，用户须显式 stop/restart；
    /// - 死记录（探测无应答且 owner.lock 可取）→ 就地离线收束（同请求
    ///   幂等回执），用户无需删除状态文件；
    /// - 在途/持久停止 → 按原请求续行至收束（不新造身份）；
    /// - 锁被持有但通道无应答 → 活而通道损坏，明确拒绝；
    /// - 仅观察失败（记录不可读）→ 如实上报，不假装没有目标。
    pub async fn ensure_no_local_execution(
        &self,
        project_id: &str,
        workspace: &Path,
    ) -> AppResult<()> {
        /// 短预算在线探测（control 自身 connect 2s；总预算收紧避免拖慢受理）。
        const ALIVE_PROBE_BUDGET: Duration = Duration::from_secs(3);
        let pending = self.pending_local_stops(project_id)?;
        let mut roots: Vec<_> = pending
            .iter()
            .map(|(_, attempt)| attempt.root.clone())
            .collect();
        if let Some(root) = lock(&self.launches)?
            .get(project_id)
            .and_then(|launch| launch.state_root.clone())
        {
            roots.push(root);
        }
        // Continue retained requests even if their latest snapshot is Stopped
        // or their root is absent from the current registry/layout.
        for (key, attempt) in pending {
            self.stop_seated_target(
                &key,
                &attempt.root,
                &attempt.binding,
                Duration::from_secs(self.config.dev_supervision_stop_budget_secs.max(1)),
            )
            .await?;
        }
        let report = discovery::discover_targets_with(workspace, &roots);
        for target in &report.targets {
            if target.snapshot.phase == runtime_supervisor::Phase::Stopped {
                continue;
            }
            let probe = runtime_supervisor::control(
                &target.state_root,
                runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
            );
            match tokio::time::timeout(ALIVE_PROBE_BUDGET, probe).await {
                Ok(Ok(_snapshot)) => {
                    // recovery v2 T2：应答原生控制的活目标若同时提供运行身份
                    //（统一 serve owner），Start 交由 start_dev 的 owner 复用
                    // 路由处理——前置检查不再把常驻管理 owner 当"活 run"拒绝
                    //（builder 固定 serve 后的必经形态）。仅应答 legacy 部署
                    // 状态、无运行 API 的本地 run 维持拒绝（不猜杀）。
                    let owner_addr = self.config.app_cli_admin_probe_addr.clone();
                    match crate::service::dev_server::owner_client::probe_owner(&owner_addr).await {
                        Ok(Some(_identity)) => {
                            tracing::info!(
                                project_id,
                                root = %target.state_root.display(),
                                "live managed owner detected; routing start through owner reuse"
                            );
                            continue;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            // 初始化中的 owner（统一 owner 首个业务会话窗口）
                            // 放行：start_dev 的复用路径对 Initializing 做有界
                            // 等待（wait_for_recovery_owner），比此处直接拒绝
                            // 更准确。其他探错维持安全方向拒绝。
                            if error.to_string().contains("initializing") {
                                tracing::info!(
                                    project_id,
                                    %error,
                                    "owner initializing during start precheck; deferring to bounded reuse wait"
                                );
                                continue;
                            }
                            tracing::warn!(%error, "owner identity probe failed during start precheck");
                        }
                    }
                    return Err(AppError::business(format!(
                        "a local orchestrator for this project is still running ({}); stop or restart it before starting a new one",
                        target.snapshot.diagnostic()
                    )));
                }
                Ok(Err(error))
                    if error
                        .downcast_ref::<runtime_supervisor::Problem>()
                        .is_some() =>
                {
                    return Err(AppError::business(format!(
                        "local orchestrator control is incompatible ({}); stop it before starting a new one",
                        error
                    )));
                }
                // 无应答（探测超时/连接失败，含"execution owner has exited"）：
                // 死记录或通道损坏——以 owner.lock 为准。
                Ok(Err(_)) | Err(_) => {
                    match runtime_supervisor::Owner::try_acquire(&target.state_root) {
                        Ok(Some(owner)) => {
                            let request = runtime_supervisor::Request::new(
                                runtime_supervisor::Action::StopWork,
                            );
                            let cleanup = self
                                .owner_cleanup_command(workspace, &target.state_root)
                                .map_err(|error| {
                                    AppError::owner_error(
                                        "prepare stale owner cleanup command",
                                        error,
                                    )
                                })?;
                            let result = owner
                                .stop_offline_with_cleanup(target.binding(), &request, &cleanup)
                                .await
                                .map_err(|error| {
                                    AppError::owner_error(
                                        "collect stale local orchestrator record",
                                        error,
                                    )
                                })?;
                            if result.phase != runtime_supervisor::Phase::Stopped {
                                return Err(AppError::owner_error(
                                    "collect stale local orchestrator record",
                                    result.recovery_error(),
                                ));
                            }
                            tracing::info!(
                                project_id,
                                root = %target.state_root.display(),
                                "collected stale local orchestrator record before start"
                            );
                        }
                        Ok(None) => {
                            return Err(AppError::business(format!(
                                "an orchestrator for this project is alive but its control channel is unavailable ({}); retry stop first",
                                target.snapshot.diagnostic()
                            )));
                        }
                        Err(error) => {
                            return Err(AppError::owner_error(
                                "inspect local orchestrator ownership",
                                error,
                            ));
                        }
                    }
                }
            }
        }
        if report.targets.is_empty() && !report.unreadable.is_empty() {
            let unreadable = report
                .unreadable
                .iter()
                .map(|(root, error)| format!("{}: {error}", root.display()))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(AppError::business(format!(
                "local supervision state is unreadable; cannot confirm no execution ({unreadable})"
            )));
        }
        Ok(())
    }
}

/// DEV-1 Fix A：平台身份 → 子进程 env 键的纯决策函数（无 env 读取，便于
/// 反例测试）。身份匹配时透传 PROJECT_ID 与显式根（无根仍透传身份）；不
/// 匹配/无平台变量返回 None（standalone 语义，registry 段继续生效）。
/// 迁移回执目录注入不在此处（DEV-R5：见 apply_migration_receipts_binding）。
pub(super) fn platform_launch_env(
    project_id_env: Option<&std::ffi::OsStr>,
    state_root_env: Option<&std::ffi::OsStr>,
    project_path: &Path,
    env_extra: &mut Vec<(String, String)>,
) -> Option<std::path::PathBuf> {
    let matches_project = project_id_env
        .filter(|value| !value.is_empty())
        .map(|project| {
            runtime_state_layout::resolve_project_origin(project_path)
                .ok()
                .and_then(|origin| origin.file_name().map(|name| name == project))
                .unwrap_or(false)
        })
        .unwrap_or(false);
    if !matches_project {
        return None;
    }
    let app_id = project_id_env
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    env_extra.push(("PROJECT_ID".to_string(), app_id));
    let root = state_root_env
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string_lossy().to_string());
    if let Some(root) = &root {
        env_extra.push(("APP_CLI_STATE_ROOT".to_string(), root.clone()));
    }
    root.map(std::path::PathBuf::from)
}

/// Managed authority is project-scoped; do not add these keys to the global
/// minimal-env allowlist used by unrelated projects and generic commands.
pub(super) fn append_managed_launch_env(
    workspace: &Path,
    state_root: &Path,
    env_extra: &mut Vec<(String, String)>,
    lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> anyhow::Result<()> {
    if let Some(managed) =
        runtime_state_layout::ManagedWorkspace::from_values(workspace, state_root, lookup)?
    {
        env_extra.extend([
            ("SERVICE_TYPE".into(), "userapp-builder".into()),
            ("APP_CLI_MANAGED".into(), "1".into()),
            (
                "APP_CLI_RUNTIME_WORKSPACE".into(),
                managed.source_root.to_string_lossy().into_owned(),
            ),
        ]);
    }
    Ok(())
}

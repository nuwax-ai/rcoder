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
        hooks: Option<crate::service::dev_server::supervise::DevEventHooks>,
        pg: Option<&shared_types::StartPgCredential>,
        request_context: Option<&str>,
    ) -> AppResult<StartedDev> {
        // 单一来源 shared_types::APP_ENTRY_PORT（release 流程、Pingora 免端口代理同值）
        const PINGAP_ENTRY_PORT: u16 = shared_types::APP_ENTRY_PORT;
        if let Some(process) = lock(&self.processes)?.get(project_id).cloned()
            && process.external_owner.is_none()
        {
            return Err(AppError::business(
                "local orchestrator is already registered; readiness must be confirmed or an explicit restart requested",
            ));
        }
        // DEV-1 §3.4：守卫扩展——除本地登记外，发现阶段的活监督目标同样阻止
        // 全新 Start（Restart 经停止收束后到这里时登记与目标均已清净）。
        // Legacy run 无 runtime API 可复用，越过守卫只会撞 3010 端口。
        if discovery::discover_targets(project_path)
            .iter()
            .any(|target| target.snapshot.phase != runtime_supervisor::Phase::Stopped)
        {
            return Err(AppError::business(
                "a local orchestrator for this project is still running; stop or restart it before starting a new one",
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
        if let Some(started) = self
            .reuse_or_refuse_owner(
                project_id,
                project_path,
                hooks.clone(),
                pg,
                None,
                request_context,
            )
            .await?
        {
            return Ok(started);
        }

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

        // DEV-1 §3.1：spawn 已发生、探活未过的窗口先发表可捕获的 Starting
        // 记录（launch 身份 + 进程登记）——并发 Stop 不得在此窗口看到"空
        // 记录"而误判无目标；失败路径按同一身份自清，不补登记成运行成功。
        let launch_id = uuid::Uuid::now_v7().simple().to_string();
        lock(&self.processes)?.insert(
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
        lock(&self.launches)?.insert(
            project_id.to_string(),
            super::super::types::LocalLaunch {
                launch_id: launch_id.clone(),
                state_root: platform_state_root,
            },
        );

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
            lock(&self.processes)?.remove(project_id);
            lock(&self.launches)?.remove(project_id);
            return Err(error);
        }

        lock(&self.supervised)?.insert(project_id.to_string(), supervised);

        Ok(StartedDev {
            pid,
            port: PINGAP_ENTRY_PORT,
        })
    }
}

/// DEV-1 Fix A：平台身份 → 子进程 env 键的纯决策函数（无 env 读取，便于
/// 反例测试）。身份匹配时透传 PROJECT_ID 与显式根（无根仍透传身份）；不
/// 匹配/无平台变量返回 None（standalone 语义，registry 段继续生效）。
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
    // 迁移回执连续性（§3.2 最小原则）：旧运行（无显式根）把回执放在
    // workspace 父目录的共享目录；该目录存在时锁定旧物理位置，已完成
    // 迁移不重跑、未确认迁移不被路径切换隐藏。不复制、不归属共享目录内容。
    if root.is_some()
        && let Some(old) = discovery::legacy_migration_receipts_dir(project_path)
    {
        env_extra.push(("APP_CLI_MIGRATION_RECEIPTS_DIR".to_string(), old));
    }
    root.map(std::path::PathBuf::from)
}

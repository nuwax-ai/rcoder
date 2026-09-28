use super::*;

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
                instance_id: None,
                base_path: None,
                started_at: now,
                log_dir: ldir.clone(),
                temp_log_name: log::temp_log_name(now),
                external_owner: None,
            },
        );
        lock(&self.supervised)?.insert(project_id.to_string(), supervised);

        Ok(StartedDev {
            pid,
            port: PINGAP_ENTRY_PORT,
        })
    }
}

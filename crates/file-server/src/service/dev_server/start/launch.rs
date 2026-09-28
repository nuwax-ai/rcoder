use super::*;

/// 启动锁守卫: drop 时自动从 starting 集合移除 (防 early return 泄漏)。
pub(crate) struct StartingGuard<'a> {
    pub(crate) starting: &'a Mutex<HashSet<String>>,
    pub(crate) project_id: String,
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
        if self.armed
            && let Err(e) = self.pool.release(&self.project_id)
        {
            tracing::warn!(project_id = %self.project_id, "port release failed in Drop: {e}");
        }
    }
}

impl DevServerManager {
    /// start-dev (对齐 nuwax startDevServer)。
    ///
    /// `hooks`：app-cli 编排事件钩子（仅 manifest 引擎消费——Userapp dev
    /// 链路转发任务 SSE + 流结束通知；web 域 vite 路径传 None）。
    /// `pg`：PG 数据库凭据（仅 manifest 引擎消费——注入编排进程 env 的
    /// POSTGRES_USER/POSTGRES_PASSWORD，覆盖容器默认透传；web 域/keep_alive
    /// 等无凭据来源的调用方传 None，维持透传行为）。
    pub async fn start_dev(
        &self,
        project_id: &str,
        project_path: &Path,
        base_path: Option<&str>,
        hooks: Option<crate::service::dev_server::supervise::DevEventHooks>,
        pg: Option<&shared_types::StartPgCredential>,
        request_context: Option<&str>,
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
        self.start_dev_inner(
            project_id,
            project_path,
            base_path,
            hooks,
            pg,
            request_context,
        )
        .await
    }

    pub(super) async fn start_dev_inner(
        &self,
        project_id: &str,
        project_path: &Path,
        base_path: Option<&str>,
        hooks: Option<crate::service::dev_server::supervise::DevEventHooks>,
        pg: Option<&shared_types::StartPgCredential>,
        request_context: Option<&str>,
    ) -> AppResult<StartedDev> {
        // Userapp workspace 分流：workspace.manifest.toml 存在 → app-cli 引擎。
        // manifest 多服务（Java/Go 等）的正确运行态 = app-cli 按 run.command
        // 编排全栈 + pingap 9080 统一入口（[proxy] 路由）；开发容器 per-app，
        // 9080 无冲突。原 package.json/vite 引擎是 web 域（单 vite dev server）
        // 移植，对多服务模板不适用——web/computer 项目（无 workspace manifest）
        // 继续走原路径。
        let manifest = project_path.join("workspace.manifest.toml");
        if tokio::fs::try_exists(&manifest).await.unwrap_or(false) {
            return self
                .start_dev_manifest(project_id, project_path, hooks, pg, request_context)
                .await;
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

        let started = self
            .spawn_and_register(project_id, project_id, project_path, port, base_path, None)
            .await?;
        port_alloc.disarm(); // 分配成功且就绪, 不再归还端口 (Drop 变 no-op)
        Ok(started)
    }

    /// 共享启动管线（legacy start 与协调票据模式共用）：npmrc → dev-inject →
    /// 依赖安装 → spawn → 就绪轮询 → 登记。
    ///
    /// `registry_key`：processes 表键（legacy=project_id；协调票据=preview_key）。
    /// `log_key`：日志目录命名键（恒 project_id，get-dev-log 兼容）。
    /// `instance_id`：协调票据的实例身份（legacy 为 None）。
    pub(crate) async fn spawn_and_register(
        &self,
        registry_key: &str,
        log_key: &str,
        project_path: &Path,
        port: u16,
        base_path: Option<&str>,
        instance_id: Option<&str>,
    ) -> AppResult<StartedDev> {
        let ldir = log::log_dir(&self.config, log_key);
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
        // N08：注入命令是 sh 语法（set +e/; 分隔）——Windows 无 sh，显式跳过
        // （非阻塞语义下 warn 可见，不假装执行成功）
        #[cfg(not(windows))]
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
        #[cfg(windows)]
        tracing::warn!(
            "dev-inject/design-mode preCmd skipped on Windows (shell-script based              injector); design mode unavailable in this environment"
        );

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
        // 端口归还由调用方守卫（legacy AllocGuard / 协调器存储端口记账）。避免把
        // "启动失败已死的 vite"当成成功。
        self.poll_alive(
            pid,
            port,
            base_path,
            &stderr_ring,
            &|port, base, timeout_ms| Box::pin(process::is_project_alive(port, base, timeout_ms)),
        )
        .await?;

        // 探活 base：显式值规范化（与 build_dev_args 同规则）或按端口组合的默认值
        let effective_base = base_path
            .map(process::normalize_base_path)
            .unwrap_or_else(|| format!("/proxy/{port}/"));
        lock(&self.processes)?.insert(
            registry_key.to_string(),
            DevProcess {
                pid,
                port,
                project_id: log_key.to_string(),
                started_at: now,
                instance_id: instance_id.map(str::to_string),
                base_path: Some(effective_base),
                log_dir: ldir.clone(),
                temp_log_name: log::temp_log_name(now),
                external_owner: None,
            },
        );

        Ok(StartedDev { pid, port })
    }
}

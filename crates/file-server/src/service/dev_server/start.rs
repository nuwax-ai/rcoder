//! dev server 启动: start_dev / start_dev_inner / poll_alive + 端口/启动守卫。

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::error_classify::{STDERR_RING_CAP, StderrRing};
use super::log;
use super::port_pool::PortPool;
use super::process;
use super::supervise::SupervisedChild;
use super::support::{early_exit_err, ldrtemp, lock, read_dev_script};
use super::types::{AliveProbe, DevServerManager, StartedDev};
use crate::error::{AppError, AppResult};
use crate::models::{DevProcess, ExternalOwner};
use crate::service::pnpm::{self, InstallOptions, LogFiles};
use std::path::Path;

/// 启动锁守卫: drop 时自动从 starting 集合移除 (防 early return 泄漏)。
pub(super) struct StartingGuard<'a> {
    pub(super) starting: &'a Mutex<HashSet<String>>,
    pub(super) project_id: String,
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
        hooks: Option<super::supervise::DevEventHooks>,
        pg: Option<&shared_types::StartPgCredential>,
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
        self.start_dev_inner(project_id, project_path, base_path, hooks, pg)
            .await
    }

    pub(super) async fn start_dev_inner(
        &self,
        project_id: &str,
        project_path: &Path,
        base_path: Option<&str>,
        hooks: Option<super::supervise::DevEventHooks>,
        pg: Option<&shared_types::StartPgCredential>,
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
                .start_dev_manifest(project_id, project_path, hooks, pg)
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
    pub(super) async fn spawn_and_register(
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

    /// Userapp workspace 的 dev 启动（app-cli 引擎）：spawn 常驻 `app-cli run
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
        hooks: Option<super::supervise::DevEventHooks>,
        pg: Option<&shared_types::StartPgCredential>,
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
            .reuse_or_refuse_owner(project_id, project_path, hooks.clone(), pg, None)
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
            let pipe_hooks = hooks.unwrap_or_else(super::supervise::DevEventHooks::noop);
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

    /// P3-02：探测 3010 的既有 owner——三种结局：
    /// - `Ok(None)`：无人监听 → 调用方走本地 spawn（legacy 路径不变）；
    /// - `Ok(Some(started))`：匹配本 workspace 的 serve owner → 经运行 API
    ///   提交 Restart(source) 复用，登记 external DevProcess；
    /// - `Err`：legacy app-cli（无 runtime API）、foreign 应答、协议不兼容、
    ///   凭据缺失——明确诊断拒绝，不杀对方、不盲 spawn（XP04）。
    ///
    /// 复用提交的运行形态（R03）：Source = 平台已就绪的 workspace 直接编排；
    /// Artifact = 平台只登记制品（共享卷 builds/ zip），激活由 owner 在
    /// 身份/revision 核验后执行——提交拒绝不改变 active 运行目录。
    async fn reuse_or_refuse_owner(
        &self,
        project_id: &str,
        project_path: &Path,
        hooks: Option<super::supervise::DevEventHooks>,
        pg: Option<&shared_types::StartPgCredential>,
        artifact_release_id: Option<&str>,
    ) -> AppResult<Option<StartedDev>> {
        let owner_addr = self.config.app_cli_admin_probe_addr.clone();
        let Some(identity) = super::owner_client::probe_owner(&owner_addr).await else {
            // 无 runtime identity：区分"legacy app-cli 应答"与"无人监听"——
            // /v1/deploy/status 是 app-cli 专属路由（foreign 服务 404）
            if legacy_app_cli_responds(&owner_addr).await {
                return Err(AppError::business(
                    "admin port 3010 is held by a legacy app-cli process without the \
                     runtime API; stop it before starting a managed instance",
                ));
            }
            return Ok(None);
        };

        if !super::owner_client::protocol_compatible(&identity) {
            return Err(AppError::business(format!(
                "app-cli owner at 3010 speaks incompatible protocol v{} (expected v{}); \
                 upgrade it before reuse",
                identity.protocol_version,
                shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
            )));
        }
        let app_id = std::env::var("PROJECT_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "unknown-app".to_string());
        super::owner_client::verify_project_identity(&identity, project_path, &app_id)
            .map_err(|error| AppError::business(format!("owner identity rejected: {error:#}")))?;
        let expected_ws = identity.workspace_id.clone();

        // 匹配 owner：读凭据（源码/产物两种状态根落点都探测；owner 未
        // 启用写端点 → 无法路由，明确报错）
        let Some((_state_root, token)) =
            super::owner_client::find_owner_token(project_path, &app_id)
        else {
            return Err(AppError::business(
                "workspace is already managed by an app-cli owner whose runtime API \
                 credentials are unavailable (APP_CLI_DEPLOY_TOKEN not enabled); \
                 stop it or restart it with the token enabled",
            ));
        };
        let client = super::owner_client::OwnerClient::new(&owner_addr, &token)
            .map_err(|error| AppError::system(format!("build owner client: {error:#}")))?;
        // P3-03：构建前捕获的期望优先（构建期间 owner 变更 → 提交被拒，
        // 不自动刷新重发）；未捕获（直接 start 无构建段）→ 提交时活取。
        let expected = lock(&self.owner_expectations)?.remove(project_id);
        // P3-03/R07：观察失败（构建期 owner 在但身份/凭据/状态读不到）→
        // 明确拒绝——不能用"没捕获到"刷新期望绕过停止屏障（R07 反例：
        // 预检断连 → 用户 Stop → 网络恢复 → 旧构建迟到提交必须被拒）。
        if let Some(super::types::OwnerExpectation::ObservationFailed { reason }) = &expected {
            return Err(AppError::business(format!(
                "owner observation failed before this build ({reason}); refusing to \
                 submit without a verified admission context — stop intent must not \
                 be bypassed by a late build"
            )));
        }
        let captured = match expected {
            Some(super::types::OwnerExpectation::Captured {
                runtime_instance_id,
                revision,
            }) => Some((runtime_instance_id, revision)),
            _ => None,
        };
        let hooks_line_for_drain = hooks.as_ref().map(|hooks| hooks.on_line.clone());
        let route_restart = async {
            let (expected_instance, expected_revision) = match captured {
                Some(captured) => captured,
                None => {
                    let status = client.status().await?;
                    (identity.runtime_instance_id.clone(), status.revision)
                }
            };
            let operation_id = format!("fs-restart-{}", uuid::Uuid::new_v4().simple());
            match artifact_release_id {
                Some(artifact_id) => {
                    client
                        .submit_restart_artifact(
                            &operation_id,
                            &expected_ws,
                            expected_revision,
                            &expected_instance,
                            pg,
                            artifact_id,
                        )
                        .await?;
                }
                None => {
                    client
                        .submit_restart_source(
                            &operation_id,
                            &expected_ws,
                            expected_revision,
                            &expected_instance,
                            pg,
                        )
                        .await?;
                }
            }
            // R06：事件转发（游标重放轮询，替换 SSE 长连——无总超时/EOF 竞态，
            // 断线续传天然支持）
            let events_client = super::owner_client::OwnerClient::new(&owner_addr, &token)?;
            let hooks_line = hooks.as_ref().map(|hooks| hooks.on_line.clone());
            let stop_token = tokio_util::sync::CancellationToken::new();
            let cursor = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let stream_operation_id = operation_id.clone();
            let stream_task = tokio::spawn({
                let stop_token = stop_token.clone();
                let cursor = cursor.clone();
                async move {
                    let forward = move |json: String| {
                        if let Some(on_line) = hooks_line.as_ref() {
                            on_line(&json);
                        }
                    };
                    super::owner_client::forward_operation_events(
                        &events_client,
                        &stream_operation_id,
                        forward,
                        &stop_token,
                        &cursor,
                        Duration::from_millis(250),
                    )
                    .await;
                }
            });
            let terminal = client
                .wait_terminal(&operation_id, Duration::from_secs(180))
                .await;
            // R06：终态先停转发并 join（不 abort——游标一致无重复投递），
            // 再按游标排空剩余事件——终态事件在状态可见后才落 journal，
            // 盲目中止会丢平台的 Done 终局。
            stop_token.cancel();
            let _joined = stream_task.await;
            if terminal.is_ok()
                && let Some(on_line) = hooks_line_for_drain.clone()
            {
                let last = cursor.load(std::sync::atomic::Ordering::SeqCst);
                if let Err(error) = super::owner_client::drain_operation_events(
                    &client,
                    &operation_id,
                    last,
                    |json| on_line(&json),
                )
                .await
                {
                    tracing::warn!("owner terminal event drain failed: {error:#}");
                }
            }
            terminal
        };
        let view = route_restart
            .await
            .map_err(|error| AppError::business(format!("reuse existing owner: {error:#}")))?;
        match view.state {
            shared_types::RuntimeOperationState::Succeeded => {}
            // R04：Cancelled 不等价成功——启动被取消既不证明编排成功也无运行
            // 证据，不登记 external（重试按新操作提交）。
            shared_types::RuntimeOperationState::Cancelled => {
                return Err(AppError::business(format!(
                    "owner restart was cancelled (operation {}); service start not confirmed",
                    view.operation_id
                )));
            }
            other => {
                return Err(AppError::business(format!(
                    "owner restart failed: {other:?} ({})",
                    view.error_message.as_deref().unwrap_or("no detail"),
                )));
            }
        }

        // 登记 external DevProcess：停止/后续操作经运行 API 路由
        // （pid 0 哨兵——外部 owner 无本地子进程句柄）。
        let now = process::now_ms();
        let ldir = log::log_dir(&self.config, project_id);
        lock(&self.processes)?.insert(
            project_id.to_string(),
            DevProcess {
                pid: 0,
                port: shared_types::APP_ENTRY_PORT,
                project_id: project_id.to_string(),
                started_at: now,
                instance_id: None,
                base_path: None,
                log_dir: ldir,
                temp_log_name: log::temp_log_name(now),
                external_owner: Some(ExternalOwner {
                    address: owner_addr,
                    token,
                    runtime_instance_id: identity.runtime_instance_id.clone(),
                }),
            },
        );
        // R05：external 控制关系持久化——file-server 重启后 stop 仍路由同一 owner
        self.persist_external_state();
        Ok(Some(StartedDev {
            pid: 0,
            port: shared_types::APP_ENTRY_PORT,
        }))
    }

    /// R03：制品态的 owner 路由决策——**激活权归属 owner**。
    /// - `Ok(Some(started))`：匹配 owner 已受理 Restart(ArtifactId) 并确认
    ///   Succeeded（owner 侧完成校验/解压/激活/编排）；平台**不触碰 .run**。
    /// - `Ok(None)`：无 owner——调用方走本地激活 + spawn（legacy 路径）。
    /// - `Err`：owner 拒绝（身份/revision/凭据）——`.run` 保持原样（提交
    ///   拒绝不改变 active 内容），错误如实上抛。
    pub async fn route_artifact_restart(
        &self,
        project_id: &str,
        workspace: &Path,
        release_id: &str,
        hooks: Option<super::supervise::DevEventHooks>,
        pg: Option<&shared_types::StartPgCredential>,
    ) -> AppResult<Option<StartedDev>> {
        // owner（产物态 serve 绑定 {ws}/.run，workspace_id=".run"）
        let run_dir = workspace.join(".run");
        self.reuse_or_refuse_owner(project_id, &run_dir, hooks, pg, Some(release_id))
            .await
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

/// legacy app-cli 探测：/v1/deploy/status 是 app-cli 专属路由——200 且
/// 响应含 protocol_version 即为 app-cli 管理面（legacy 或 serve 形态）；
/// foreign 服务 404/异构 body → false。
pub(super) async fn legacy_app_cli_responds(address: &str) -> bool {
    let url = format!("http://{address}/v1/deploy/status");
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .no_proxy()
        .build()
    else {
        return false;
    };
    let Ok(response) = client.get(&url).send().await else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    response
        .json::<serde_json::Value>()
        .await
        .is_ok_and(|body| body.get("protocol_version").is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 假编排器测试装配：脚本 dump 自身 env 后驻留 60s（宽松就绪——HTTP 探不到
    /// 但进程存活即过，与 vite 路径同语义）。返回 env dump 文件路径。
    fn fake_orchestrator(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let dump = dir.join("env.txt");
        let script = dir.join("fake-app-cli.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\n[ \"$1\" = run ] && [ \"$2\" = --workspace ] || exit 2\nenv | sort > '{}'\nsleep 60\n", dump.display()),
        )
        .expect("write fake orchestrator");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod script");
        }
        (dump, script)
    }

    fn manager_with(script: &Path, logs: &Path) -> DevServerManager {
        let mut config = crate::Config::from_env().expect("test config");
        config.app_cli_bin = Some(script.display().to_string());
        config.log_base_dir = logs.to_path_buf();
        // 探测地址指向必然拒绝连接的端口：spawn 前探测恒 fallthrough，
        // 避免与 owner_probe_branches（占用 3010）跨测试进程竞争
        config.app_cli_admin_probe_addr = "127.0.0.1:1".to_string();
        // 快速宽松就绪（假编排器不监听 9080，走"进程存活"分支）
        config.dev_alive_max_wait_ms = 300;
        config.dev_alive_check_timeout_ms = 100;
        config.dev_alive_poll_interval_ms = 50;
        DevServerManager::new(Arc::new(config))
    }

    /// 等待假编排器落盘 env dump（spawn 异步于断言）。
    async fn wait_dump(dump: &Path) -> String {
        for _ in 0..200 {
            if let Ok(txt) = std::fs::read_to_string(dump)
                && !txt.is_empty()
            {
                return txt;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("fake orchestrator env dump not written: {}", dump.display());
    }

    /// pg 凭据注入：start_dev_manifest(Some) 必须把 POSTGRES_USER/PASSWORD
    /// 写进编排进程 env（覆盖容器默认透传）——save-db-credential 改密后的
    /// 新凭据经此到达服务进程；None 则维持容器 env 透传行为（父进程无该键
    /// 时结果里不出现）。
    #[tokio::test]
    async fn start_dev_manifest_pg_credential_env_injection() {
        let ws = tempfile::tempdir().expect("workspace tempdir");
        // manifest 分流标记（内容不消费——分流只看存在性；假编排器不读它）
        std::fs::write(ws.path().join("workspace.manifest.toml"), "").expect("manifest marker");
        let dir = tempfile::tempdir().expect("harness tempdir");
        let (dump, script) = fake_orchestrator(dir.path());
        let manager = manager_with(&script, &dir.path().join("logs"));

        // Some(pg)：注入覆盖
        let pg = shared_types::StartPgCredential {
            username: "biz_user".into(),
            password: "s3cret".into(),
        };
        let started = manager
            .start_dev("pg-inject-test", ws.path(), None, None, Some(&pg))
            .await
            .expect("start manifest dev");
        assert_eq!(started.port, shared_types::APP_ENTRY_PORT);
        let env_txt = wait_dump(&dump).await;
        for expect in [
            "POSTGRES_USER=biz_user",
            "POSTGRES_PASSWORD=s3cret",
            "APP_CLI_RUN_PROFILE=dev",
        ] {
            assert!(
                env_txt.lines().any(|l| l == expect),
                "expect {expect} in orchestrator env:\n{env_txt}"
            );
        }
        manager.stop_dev("pg-inject-test").await.expect("stop dev");

        // None：不注入——期望值 = 父进程 env 透传结果（无则不出现）
        std::fs::remove_file(&dump).expect("reset dump");
        let started = manager
            .start_dev("pg-inject-none-test", ws.path(), None, None, None)
            .await
            .expect("start manifest dev (no pg)");
        assert_eq!(started.port, shared_types::APP_ENTRY_PORT);
        let env_txt = wait_dump(&dump).await;
        for (key, injected) in [
            ("POSTGRES_USER", "biz_user"),
            ("POSTGRES_PASSWORD", "s3cret"),
        ] {
            let lines: Vec<String> = env_txt
                .lines()
                .filter(|l| l.starts_with(&format!("{key}=")))
                .map(|l| l.to_string())
                .collect();
            match std::env::var(key) {
                Ok(parent) => {
                    // 白名单透传：值 = 父进程值（绝不能是注入值）
                    assert_eq!(lines, vec![format!("{key}={parent}")], "passthrough {key}");
                }
                Err(_) => assert!(lines.is_empty(), "{key} 不得凭空出现: {lines:?}"),
            }
            assert!(
                !lines.iter().any(|l| l == &format!("{key}={injected}")),
                "None 时不得注入 {key}"
            );
        }
        manager
            .stop_dev("pg-inject-none-test")
            .await
            .expect("stop dev");
    }
}

#[cfg(test)]
mod owner_reuse_tests {
    use super::*;
    use crate::Config;
    use std::sync::Arc;

    /// P3-02 探测分支（串行四阶段——nextest 每测试独立进程，3010 需独占）：
    /// ① 无监听 → fallthrough spawn；② legacy 应答 → 拒绝；
    /// ③ 匹配 owner + token → 经运行 API 复用并登记 external；
    /// ④ foreign workspace → 拒绝。
    #[tokio::test]
    async fn owner_probe_branches() {
        let mgr = DevServerManager::new(Arc::new(Config::from_env().expect("test config")));
        let dir = tempfile::tempdir().unwrap();

        // ① 无监听（此阶段 3010 必须空闲——串行前提）
        let ws_fresh = dir.path().join("ws-fresh");
        std::fs::create_dir_all(&ws_fresh).unwrap();
        assert!(
            mgr.reuse_or_refuse_owner("userapp:app", &ws_fresh, None, None, None)
                .await
                .expect("probe")
                .is_none(),
            "no responder must fall through to spawn"
        );

        // ② legacy app-cli 应答（仅 /v1/deploy/status）
        let legacy = axum::Router::new().route(
            "/v1/deploy/status",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({"protocol_version": 4, "phase": "running"}))
            }),
        );
        let legacy_mock = serve_mock(legacy).await;
        let ws_legacy = dir.path().join("ws-legacy");
        std::fs::create_dir_all(&ws_legacy).unwrap();
        let error = mgr
            .reuse_or_refuse_owner("userapp:app", &ws_legacy, None, None, None)
            .await
            .expect_err("legacy responder must be refused");
        let AppError::Business(message) = &error else {
            panic!("expected business error, got {error:?}")
        };
        assert!(
            message.contains("legacy app-cli"),
            "diagnostic should name legacy: {message}"
        );

        legacy_mock.release().await;

        // ③ 匹配 owner + token 文件 → 复用
        let ws_reuse = dir.path().join("ws-reuse");
        std::fs::create_dir_all(&ws_reuse).unwrap();
        let state_root = dir.path().join(".app-cli-state").join("unknown-app");
        std::fs::create_dir_all(&state_root).unwrap();
        std::fs::write(state_root.join("token"), "test-token").unwrap();
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let owner = mock_owner_router("hashed-workspace", &ws_reuse).with_state(polls);
        let owner_mock = serve_mock(owner).await;
        let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = received.clone();
        let hooks = super::super::supervise::DevEventHooks {
            on_line: std::sync::Arc::new(move |json: &str| {
                sink.lock().unwrap().push(json.to_string());
            }) as process::OnLineCallback,
            on_end: None,
        };
        let started = mgr
            .reuse_or_refuse_owner("userapp:app", &ws_reuse, Some(hooks), None, None)
            .await
            .expect("reuse path")
            .expect("must reuse existing owner");
        assert_eq!(started.port, shared_types::APP_ENTRY_PORT);
        // R06：事件按游标重放转发为旧 EVT 形态——服务级事件透传、纯 stage
        // 记录跳过、终态 Completed 映射为平台 orchestration_done（真实 owner
        // 成功也产生 Done 终局）
        let (got_done, got_service) = {
            let events = received.lock().unwrap();
            (
                events.iter().any(|json| {
                    json.contains("\"event\":\"orchestration_done\"")
                        && json.contains("\"failed\":[]")
                }),
                events.iter().any(|json| {
                    json.contains("\"event\":\"service_starting\"") && json.contains("\"frontend\"")
                }),
            )
        };
        assert!(
            got_done,
            "terminal Completed must map to orchestration_done with empty failed; got {:?}",
            received.lock().unwrap()
        );
        assert!(
            got_service,
            "service events must pass through; got {:?}",
            received.lock().unwrap()
        );
        {
            let registered = lock(&mgr.processes).unwrap();
            let entry = registered.get("userapp:app").expect("registered");
            assert_eq!(
                entry.external_owner.as_ref().expect("external").token,
                "test-token"
            );
        }

        owner_mock.release().await;

        // ④ foreign workspace → 拒绝
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let foreign = mock_owner_router("ws-someone-else", &dir.path().join("someone-else"))
            .with_state(polls);
        let _foreign_mock = serve_mock(foreign).await;
        let ws_mine = dir.path().join("ws-mine");
        std::fs::create_dir_all(&ws_mine).unwrap();
        let error = mgr
            .reuse_or_refuse_owner("userapp:app", &ws_mine, None, None, None)
            .await
            .expect_err("foreign owner must be refused");
        let AppError::Business(message) = &error else {
            panic!("expected business error, got {error:?}")
        };
        assert!(
            message.contains("different app-cli owner"),
            "diagnostic should name the conflict: {message}"
        );
    }

    /// HttpResult 信封（wire 层格式手拼 OK；data 载荷一律 typed 构造——
    /// 契约结构体字段偏移时编译器强制 mock 同步，不靠字符串检查）。
    fn envelope<T: serde::Serialize>(data: &T) -> serde_json::Value {
        serde_json::json!({"success": true, "code": "OK", "data": data, "message": "ok"})
    }

    fn operation_view(
        id: &str,
        state: shared_types::RuntimeOperationState,
    ) -> shared_types::RuntimeOperationView {
        shared_types::RuntimeOperationView {
            operation_id: id.to_string(),
            kind: shared_types::RuntimeOperationKind::Restart,
            state,
            request_digest: "digest".to_string(),
            revision: 1,
            runtime_instance_id: "instance-test".to_string(),
            error_code: None,
            error_message: None,
            failure_detail: None,
        }
    }

    fn mock_owner_router(
        ws: &str,
        root: &Path,
    ) -> axum::Router<std::sync::Arc<std::sync::atomic::AtomicUsize>> {
        use shared_types::{
            DesiredState, ObservedHealth, RuntimeEventRecord, RuntimeIdentityView,
            RuntimeOperationState, RuntimeStatusView,
        };

        let source_root = root.to_string_lossy().into_owned();
        let identity_of = move |workspace: String| RuntimeIdentityView {
            application_id: "unknown-app".to_string(),
            service_family: "userapp-dev".to_string(),
            workspace_id: workspace,
            source_root: source_root.clone(),
            runtime_instance_id: "instance-test".to_string(),
            deployment_generation_id: "gen-test".to_string(),
            protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: Vec::new(),
        };
        let status_view = || RuntimeStatusView {
            desired: DesiredState::Running,
            observed: ObservedHealth::Ready,
            active_target: None,
            revision: 0,
            active_operation_id: None,
            recovery_protection: false,
            runtime_instance_id: "instance-test".to_string(),
        };

        let ws = ws.to_string();
        let identity_ws = ws.clone();
        axum::Router::new()
            .route(
                "/v1/runtime/identity",
                axum::routing::get(move || {
                    let identity = identity_of(identity_ws.clone());
                    async move { axum::Json(envelope(&identity)) }
                }),
            )
            .route(
                "/v1/runtime/status",
                axum::routing::get(move || async move { axum::Json(envelope(&status_view())) }),
            )
            .route(
                "/v1/runtime/operations",
                axum::routing::post(
                    |axum::Json(req): axum::Json<serde_json::Value>| async move {
                        let id = req
                            .get("operation_id")
                            .and_then(|value| value.as_str())
                            .unwrap_or("op")
                            .to_string();
                        let receipt = shared_types::RuntimeOperationAccepted {
                            operation_id: id.clone(),
                            state: RuntimeOperationState::Accepted,
                            poll: format!("/v1/runtime/operations/{id}"),
                        };
                        (
                            axum::http::StatusCode::ACCEPTED,
                            axum::Json(envelope(&receipt)),
                        )
                    },
                ),
            )
            .route(
                "/v1/runtime/operations/{id}",
                axum::routing::get(
                    |axum::extract::Path(id): axum::extract::Path<String>,
                     axum::extract::State(polls): axum::extract::State<
                        Arc<std::sync::atomic::AtomicUsize>,
                    >| async move {
                        // 首查 accepted（留事件流消费窗口），其后 succeeded
                        let seen = polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let state = if seen == 0 {
                            RuntimeOperationState::Accepted
                        } else {
                            RuntimeOperationState::Succeeded
                        };
                        axum::Json(envelope(&operation_view(&id, state)))
                    },
                ),
            )
            .route(
                "/v1/runtime/operations/{id}/events",
                axum::routing::get(
                    |axum::extract::Path(id): axum::extract::Path<String>,
                     axum::extract::Query(query): axum::extract::Query<
                        std::collections::HashMap<String, String>,
                    >| async move {
                        // typed 构造事件记录（R06 重放端点契约：after_seq 游标）：
                        // 服务级事件透传 + 无 event_name 的纯 stage（适配层跳过）
                        // + 终态 Completed（映射为平台 orchestration_done）
                        let after: u64 = query
                            .get("after_seq")
                            .and_then(|value| value.parse().ok())
                            .unwrap_or(0);
                        let all = [
                            RuntimeEventRecord {
                                operation_id: id.clone(),
                                sequence: 1,
                                runtime_instance_id: "instance-test".to_string(),
                                stage: "service".to_string(),
                                service: Some("frontend".to_string()),
                                event_name: Some("service_starting".to_string()),
                                payload: None,
                            },
                            RuntimeEventRecord {
                                operation_id: id.clone(),
                                sequence: 2,
                                runtime_instance_id: "instance-test".to_string(),
                                stage: "stage-only".to_string(),
                                service: None,
                                event_name: None,
                                payload: None,
                            },
                            RuntimeEventRecord {
                                operation_id: id.clone(),
                                sequence: 3,
                                runtime_instance_id: "instance-test".to_string(),
                                stage: "terminal".to_string(),
                                service: None,
                                event_name: Some("Completed".to_string()),
                                payload: None,
                            },
                        ];
                        let events: Vec<_> = all
                            .into_iter()
                            .filter(|record| record.sequence > after)
                            .collect();
                        axum::Json(envelope(&serde_json::json!({
                            "operation_id": id,
                            "events": events,
                        })))
                    },
                ),
            )
    }

    /// R07/R08 反例：构建期观察失败 → 迟到提交拒绝（不刷新期望绕过停止
    /// 屏障）；携带 pg 的复用提交在 wire 上包含 run_config.pg。
    /// 独立自由端口 + 可配置 probe addr（不与 owner_probe_branches 的 3010 竞争）。
    #[tokio::test]
    async fn observation_failure_blocks_late_submission_and_pg_travels_on_wire() {
        use crate::Config;
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws-r78");
        std::fs::create_dir_all(&ws).unwrap();
        let state_root = dir.path().join(".app-cli-state").join("unknown-app");
        std::fs::create_dir_all(&state_root).unwrap();
        // 刻意**先不写 token**：构建期 capture 时 owner 在但凭据缺 →
        // ObservationFailed（真实分类路径，非手工注入）
        let submitted: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = submitted.clone();
        let identity = shared_types::RuntimeIdentityView {
            application_id: "unknown-app".to_string(),
            service_family: "userapp-dev".to_string(),
            workspace_id: "ws-r78".to_string(),
            source_root: ws.to_string_lossy().into_owned(),
            runtime_instance_id: "instance-r78".to_string(),
            deployment_generation_id: "gen".to_string(),
            protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: Vec::new(),
        };
        let status_view = shared_types::RuntimeStatusView {
            desired: shared_types::DesiredState::Running,
            observed: shared_types::ObservedHealth::Ready,
            active_target: None,
            revision: 7,
            active_operation_id: None,
            recovery_protection: false,
            runtime_instance_id: "instance-r78".to_string(),
        };
        let app = axum::Router::new()
            .route(
                "/v1/runtime/identity",
                axum::routing::get(move || {
                    let identity = identity.clone();
                    async move { axum::Json(envelope(&identity)) }
                }),
            )
            .route(
                "/v1/runtime/status",
                axum::routing::get(move || async move { axum::Json(envelope(&status_view)) }),
            )
            .route(
                "/v1/runtime/operations",
                axum::routing::post(move |body: axum::Json<serde_json::Value>| {
                    let sink = sink.clone();
                    async move {
                        sink.lock().unwrap().push(body.0.clone());
                        axum::Json(envelope(&serde_json::json!({
                            "operation_id": "op-x",
                            "state": "accepted",
                        })))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let probe_addr = listener.local_addr().unwrap().to_string();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock serve");
        });

        let mut config = Config::from_env().expect("test config");
        config.app_cli_admin_probe_addr = probe_addr.clone();
        let mgr = DevServerManager::new(Arc::new(config));

        // ── R07：构建期 owner 在但凭据缺 → ObservationFailed 分类
        mgr.capture_owner_expectation("userapp:r78", &ws).await;
        {
            let map = lock(&mgr.owner_expectations).unwrap();
            match map.get("userapp:r78") {
                Some(super::super::types::OwnerExpectation::ObservationFailed { .. }) => {}
                other => panic!("expected ObservationFailed, got {other:?}"),
            }
        }
        // 网络恢复（补 token）+ 用户 Stop 已推进 revision（mock revision=7）：
        // 旧构建迟到提交必须被拒——不得刷新期望
        std::fs::write(state_root.join("token"), "test-token").unwrap();
        let error = mgr
            .reuse_or_refuse_owner("userapp:r78", &ws, None, None, None)
            .await
            .expect_err("late build submission must be refused");
        assert!(
            error.to_string().contains("observation failed"),
            "diagnostic: {error}"
        );
        assert!(
            submitted.lock().unwrap().is_empty(),
            "no operation may be submitted when the admission context is unverified"
        );

        // ── R07 对照：确认无 owner（无人监听端口）→ NoOwner 分类
        let mut config = Config::from_env().expect("test config");
        config.app_cli_admin_probe_addr = "127.0.0.1:1".to_string();
        let mgr2 = DevServerManager::new(Arc::new(config));
        mgr2.capture_owner_expectation("userapp:none", &ws).await;
        {
            let map = lock(&mgr2.owner_expectations).unwrap();
            assert!(matches!(
                map.get("userapp:none"),
                Some(super::super::types::OwnerExpectation::NoOwner)
            ));
        }

        // ── R08：清除失败期望（模拟下一轮构建成功捕获后），复用提交在
        // wire 上携带 run_config.pg（新凭据到达 owner）
        lock(&mgr.owner_expectations).unwrap().remove("userapp:r78");
        let pg = shared_types::StartPgCredential {
            username: "biz_user".to_string(),
            password: "new-s3cret".to_string(),
        };
        // 复用路径会 wait_terminal 轮询到超时——mock 不提供 operation 查询端点，
        // 这里只需断言**提交 wire**；超时错误可忽略（操作已受理记录在案）
        let _reuse_result = mgr
            .reuse_or_refuse_owner("userapp:r78", &ws, None, Some(&pg), None)
            .await;
        let posted = submitted.lock().unwrap();
        let last = posted
            .last()
            .cloned()
            .unwrap_or_else(|| panic!("restart must be submitted; got {:?}", posted));
        assert_eq!(
            last["run_config"]["pg"]["username"], "biz_user",
            "wire must carry the new pg credential: {last}"
        );
        assert_eq!(last["run_config"]["pg"]["password"], "new-s3cret");
        task.abort();
    }

    /// R03 反例：制品态 owner 路由——owner 拒绝（revision 推进）时 `.run`
    /// 保持原样（提交拒绝不改变 active 内容）；owner 受理时请求按
    /// ArtifactId 形态上 wire（不是 Source restart）。
    #[tokio::test]
    async fn artifact_owner_route_preserves_run_dir_on_rejection() {
        use crate::Config;
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws-art");
        std::fs::create_dir_all(&ws).unwrap();
        // 既有 .run（旧版本运行目录）+ 标记文件——必须原样保留
        std::fs::create_dir_all(ws.join(".run")).unwrap();
        std::fs::write(ws.join(".run").join("marker-old"), "v1").unwrap();

        // mock owner：workspace_id=".run"（产物态绑定）、token 文件、
        // 受理后固定返回 ERR_REVISION_MISMATCH 拒绝
        let state_root = dir.path().join(".app-cli-state").join("unknown-app");
        std::fs::create_dir_all(&state_root).unwrap();
        std::fs::write(state_root.join("token"), "test-token").unwrap();
        let submitted: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = submitted.clone();
        let identity = shared_types::RuntimeIdentityView {
            application_id: "unknown-app".to_string(),
            service_family: "userapp-dev".to_string(),
            workspace_id: ".run".to_string(),
            source_root: ws.join(".run").to_string_lossy().into_owned(),
            runtime_instance_id: "instance-art".to_string(),
            deployment_generation_id: "gen".to_string(),
            protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: Vec::new(),
        };
        let identity_ws = identity.clone();
        let app = axum::Router::new()
            .route(
                "/v1/runtime/identity",
                axum::routing::get(move || {
                    let identity = identity_ws.clone();
                    async move { axum::Json(envelope(&identity)) }
                }),
            )
            .route(
                "/v1/runtime/status",
                axum::routing::get(|| async {
                    let status = shared_types::RuntimeStatusView {
                        desired: shared_types::DesiredState::Running,
                        observed: shared_types::ObservedHealth::Ready,
                        active_target: None,
                        revision: 9,
                        active_operation_id: None,
                        recovery_protection: false,
                        runtime_instance_id: "instance-art".to_string(),
                    };
                    axum::Json(envelope(&status))
                }),
            )
            .route(
                "/v1/runtime/operations",
                axum::routing::post(move |body: axum::Json<serde_json::Value>| {
                    let sink = sink.clone();
                    async move {
                        sink.lock().unwrap().push(body.0.clone());
                        // 模拟构建期间用户 Stop 推进 revision → 受理拒绝
                        axum::response::Response::builder()
                            .status(409)
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from(
                                serde_json::json!({
                                    "success": false,
                                    "code": "ERR_REVISION_MISMATCH",
                                    "message": "expected revision 8 does not match current 9",
                                })
                                .to_string(),
                            ))
                            .expect("reject response")
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let mut config = Config::from_env().expect("test config");
        config.app_cli_admin_probe_addr = addr;
        let mgr = DevServerManager::new(Arc::new(config));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock serve");
        });

        // owner 在但拒绝（revision 已推进）
        let error = mgr
            .route_artifact_restart("userapp:art", &ws, "rel-new", None, None)
            .await
            .expect_err("owner rejection must propagate");
        assert!(
            error.to_string().contains("ERR_REVISION_MISMATCH")
                || error.to_string().contains("reuse"),
            "diagnostic: {error}"
        );
        // R03 核心：`.run` 原样（提交拒绝不改变 active 内容）
        assert!(
            ws.join(".run").join("marker-old").is_file(),
            "active .run must be untouched on owner rejection"
        );
        assert!(
            !ws.join(".previous").exists(),
            "no rotation may happen on rejection"
        );
        // wire：提交按 ArtifactId 形态（artifact_id 上 wire，非 Source）
        let posted = submitted.lock().unwrap();
        let last = posted
            .last()
            .cloned()
            .unwrap_or_else(|| panic!("{posted:?}"));
        // wire 形态跟随 RunProfileInput 的 serde tag（tag="profile"/
        // content="input" + ArtifactInput 的 tag="source"/content="value"）
        assert_eq!(
            last["profile"]["profile"], "artifact",
            "artifact restart must use Artifact profile: {last}"
        );
        assert_eq!(
            last["profile"]["input"]["artifact"]["source"],
            "artifact_id"
        );
        assert_eq!(
            last["profile"]["input"]["artifact"]["value"]["artifact_id"],
            "rel-new"
        );
        drop(last);
        drop(posted);
        task.abort();
    }

    /// 起 mock 在 3010；返回显式释放句柄（顺序关闭 + 等待端口回收）。
    async fn serve_mock(app: axum::Router<()>) -> MockOwner {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:3010")
            .await
            .expect("bind 3010");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .expect("mock serve");
        });
        MockOwner {
            task: Some(task),
            tx: Some(tx),
        }
    }

    struct MockOwner {
        task: Option<tokio::task::JoinHandle<()>>,
        tx: Option<tokio::sync::oneshot::Sender<()>>,
    }
    impl MockOwner {
        /// 显式关闭并等待端口回收（下一阶段绑定的前提）。
        async fn release(mut self) {
            if let Some(tx) = self.tx.take() {
                let _ = tx.send(());
            }
            if let Some(task) = self.task.take() {
                let _joined = task.await;
            }
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while tokio::net::TcpListener::bind("127.0.0.1:3010")
                .await
                .is_err()
            {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "mock owner did not release port 3010"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    impl Drop for MockOwner {
        fn drop(&mut self) {
            if let Some(task) = self.task.take() {
                task.abort();
            }
        }
    }
}

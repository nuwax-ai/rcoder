use super::*;

#[derive(Debug)]
pub(super) enum LaunchObservation {
    Ready,
    /// run 引导成功移交 serve owner：identity 为通过工作区/应用身份核验的
    /// owner 身份（登记侧据此完成外部 owner 转换——R3）。
    HandedOver {
        identity: shared_types::RuntimeIdentityView,
    },
    DeadlineExpired,
}

/// spawn 进程退出时的处置策略——两条启动路径（vite / manifest 引擎）在
/// 就绪等待上的唯一差异点。
pub(super) enum ExitPolicy<'a> {
    /// 进程常驻（vite 路径）：退出即启动失败，读 stderr 分类成可操作错误。
    Fail,
    /// `app-cli run` 是一次性引导（0.3.16+，可重复执行），常驻主体是
    /// `app-cli serve` owner（manifest 路径）：run 退出后按 **监督退出码**
    /// 与 owner 身份双证据判移交（R2：run 非零退出不被 owner 存活掩盖——
    /// 管理面按产品契约在业务失败后仍在线，owner 存在 ≠ 启动成功）；
    /// 退出码 0 且 owner 身份核验通过才确认移交，存活判据随之锚定 owner。
    ConfirmOwner {
        workspace: &'a Path,
        supervised: &'a Arc<SupervisedChild>,
    },
}

/// run 引导退出后的 owner 移交核验预算（覆盖 serve 绑定 3010 的竞态窗口；
/// 真失败的引导也会等满该预算再报错——上限可控）。
pub(super) const HANDOVER_CONFIRM_BUDGET: Duration = Duration::from_secs(2);
/// 移交核验重试间隔（兼作监督退出码收割的短等待窗）。
const HANDOVER_PROBE_RETRY: Duration = Duration::from_millis(200);

/// 就绪等待的公共参数（收拢自原 8 参签名——参数按对象成组传递）。
pub(super) struct AliveWatch<'a> {
    pub port: u16,
    pub base_path: Option<&'a str>,
    pub stderr_ring: &'a Arc<StderrRing>,
    pub probe: AliveProbe<'a>,
    pub launch_deadline: Option<tokio::time::Instant>,
    pub exit_policy: &'a ExitPolicy<'a>,
}

impl DevServerManager {
    /// 就绪轮询: 进程早退 → Err (读 stderr ring 分类成可操作错误); HTTP 就绪 → Ok;
    /// 超时但进程仍在 → Ok (nuwax 宽松)。
    pub(crate) async fn poll_alive(
        &self,
        pid: u32,
        port: u16,
        base_path: Option<&str>,
        stderr_ring: &Arc<StderrRing>,
        probe: AliveProbe<'_>,
    ) -> AppResult<()> {
        self.poll_alive_with_launch_deadline(pid, port, base_path, stderr_ring, probe, None)
            .await
            .map(|_| ())
    }

    pub(super) async fn poll_alive_with_launch_deadline(
        &self,
        pid: u32,
        port: u16,
        base_path: Option<&str>,
        stderr_ring: &Arc<StderrRing>,
        probe: AliveProbe<'_>,
        launch_deadline: Option<tokio::time::Instant>,
    ) -> AppResult<LaunchObservation> {
        self.wait_alive(
            pid,
            AliveWatch {
                port,
                base_path,
                stderr_ring,
                probe,
                launch_deadline,
                exit_policy: &ExitPolicy::Fail,
            },
        )
        .await
    }

    /// manifest 引擎启动等待：探活打 9080 根路径（按 [proxy] path 路由时 404
    /// 属常态，靠宽松语义兜底），进程退出处置按"监督退出码 + owner 移交
    /// 核验"双证据（见 [`ExitPolicy::ConfirmOwner`]）。
    pub(super) async fn wait_manifest_alive(
        &self,
        pid: u32,
        port: u16,
        workspace: &Path,
        supervised: &Arc<SupervisedChild>,
        stderr_ring: &Arc<StderrRing>,
        launch_deadline: Option<tokio::time::Instant>,
    ) -> AppResult<LaunchObservation> {
        self.wait_alive(
            pid,
            AliveWatch {
                port,
                base_path: None,
                stderr_ring,
                probe: &|port, _base, timeout_ms| {
                    Box::pin(process::is_project_alive(port, Some("/"), timeout_ms))
                },
                launch_deadline,
                exit_policy: &ExitPolicy::ConfirmOwner {
                    workspace,
                    supervised,
                },
            },
        )
        .await
    }

    /// 统一就绪等待循环：deadline 钳制、探活节奏与宽松收尾对两条路径共用；
    /// 进程退出处置（见 [`ExitPolicy`]) 是唯一分支点。
    pub(super) async fn wait_alive(
        &self,
        pid: u32,
        watch: AliveWatch<'_>,
    ) -> AppResult<LaunchObservation> {
        let AliveWatch {
            port,
            base_path,
            stderr_ring,
            probe,
            launch_deadline,
            exit_policy,
        } = watch;
        let max = self.config.dev_alive_max_wait_ms;
        let timeout = self.config.dev_alive_check_timeout_ms;
        let interval = Duration::from_millis(self.config.dev_alive_poll_interval_ms);
        // 用墙钟 deadline 计时 (而非固定步进累加): 探活本身可能耗时 (未 ready 时等到
        // timeout), 固定步进累加会让 max 超时判断严重偏松 (实际墙钟远大于累加值)。
        let deadline = std::time::Instant::now() + Duration::from_millis(max);
        let mut owner_confirmed: Option<shared_types::RuntimeIdentityView> = None;
        loop {
            if launch_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                return Ok(LaunchObservation::DeadlineExpired);
            }
            if owner_confirmed.is_none() {
                owner_confirmed = self.observe_launcher_exit(pid, &watch).await?;
            }
            // deadline 检查放探活前: 兜底 max=0 边界 + sleep 后已超 deadline 不再多探活。
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            // 探活超时夹到剩余 deadline (min): 防单次探活越过 deadline, 让 max 成硬上限
            // (默认 max=30s >> timeout=1.5s 不触发, 仅防御 max<timeout 的错误配置)。
            // 首轮即探活 (不固定盲等 sleep): spawn 返回时端口未 listen, reqwest
            // connection refused 快速失败, 等价探测式等待; 提前 ready 能立刻发现。
            let mut this_timeout = timeout.min((deadline - now).as_millis() as u64);
            if let Some(parent) = launch_deadline {
                this_timeout = this_timeout.min(
                    parent
                        .saturating_duration_since(tokio::time::Instant::now())
                        .as_millis() as u64,
                );
            }
            let observation = probe(port, base_path, this_timeout);
            let ready = match launch_deadline {
                Some(parent) => match tokio::time::timeout_at(parent, observation).await {
                    Ok(ready) => ready,
                    Err(_) => return Ok(LaunchObservation::DeadlineExpired),
                },
                None => observation.await,
            };
            if ready {
                // The launcher can exit while the HTTP probe is in flight.
                // Preserve a verified handover even when HTTP readiness succeeds;
                // otherwise its dead PID would retain a local registration.
                if owner_confirmed.is_none() {
                    owner_confirmed = self.observe_launcher_exit(pid, &watch).await?;
                }
                if launch_deadline.is_some_and(|parent| tokio::time::Instant::now() >= parent) {
                    return Ok(LaunchObservation::DeadlineExpired);
                }
                return Ok(match owner_confirmed {
                    Some(identity) => LaunchObservation::HandedOver { identity },
                    None => LaunchObservation::Ready,
                });
            }
            tokio::time::sleep_until(
                launch_deadline.map_or(tokio::time::Instant::now() + interval, |parent| {
                    parent.min(tokio::time::Instant::now() + interval)
                }),
            )
            .await;
        }
        if launch_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
            return Ok(LaunchObservation::DeadlineExpired);
        }
        // 宽松就绪（超时未响应 HTTP）：存活判据按策略锚定——Fail=进程，
        // ConfirmOwner 在移交成立后=serve owner（再核验一次），未移交=进程。
        let alive = match exit_policy {
            ExitPolicy::Fail => process::is_process_running(pid),
            ExitPolicy::ConfirmOwner {
                workspace,
                supervised,
            } => {
                // R2（宽松收尾同款）：等待窗内 run 非零退出 = 失败，不被
                // owner 存活或宽松语义掩盖。
                if let Some(exit) = supervised.exited() {
                    require_successful_run_exit(Some(exit), pid, port, stderr_ring)?;
                }
                if owner_confirmed.is_some() {
                    // 收尾复验钳制到移交预算与父 launch deadline 的较早者。
                    let mut budget = HANDOVER_CONFIRM_BUDGET;
                    if let Some(deadline) = launch_deadline {
                        budget = budget
                            .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
                    }
                    let refreshed = self.confirm_owner_handover(workspace, budget).await;
                    refreshed
                        .as_ref()
                        .zip(owner_confirmed.as_ref())
                        .is_some_and(|(current, captured)| {
                            current.runtime_instance_id == captured.runtime_instance_id
                        })
                } else {
                    process::is_process_running(pid)
                }
            }
        };
        if !alive {
            return Err(early_exit_err(pid, port, stderr_ring));
        }
        if launch_deadline.is_some_and(|parent| tokio::time::Instant::now() >= parent) {
            return Ok(LaunchObservation::DeadlineExpired);
        }
        tracing::warn!(
            "dev server on port {port} (pid {pid}) 未在 {max}ms 内响应 HTTP, 编排主体仍在 — 返回成功 (nuwax 宽松)"
        );
        Ok(match owner_confirmed {
            Some(identity) => LaunchObservation::HandedOver { identity },
            None => LaunchObservation::Ready,
        })
    }

    /// Observe the launcher independently of its numeric PID. A reaped exit
    /// remains authoritative even if the OS has reused that PID.
    async fn observe_launcher_exit(
        &self,
        pid: u32,
        watch: &AliveWatch<'_>,
    ) -> AppResult<Option<shared_types::RuntimeIdentityView>> {
        match watch.exit_policy {
            ExitPolicy::Fail => {
                if !process::is_process_running(pid) {
                    return Err(early_exit_err(pid, watch.port, watch.stderr_ring));
                }
                Ok(None)
            }
            ExitPolicy::ConfirmOwner {
                workspace,
                supervised,
            } => {
                if supervised.exited().is_none() && process::is_process_running(pid) {
                    return Ok(None);
                }
                let remaining = |budget: Duration| {
                    watch.launch_deadline.map_or(budget, |parent| {
                        budget.min(parent.saturating_duration_since(tokio::time::Instant::now()))
                    })
                };
                let exit = supervised.wait_exit(remaining(HANDOVER_PROBE_RETRY)).await;
                require_successful_run_exit(exit, pid, watch.port, watch.stderr_ring)?;
                self.confirm_owner_handover(workspace, remaining(HANDOVER_CONFIRM_BUDGET))
                    .await
                    .map(Some)
                    .ok_or_else(|| early_exit_err(pid, watch.port, watch.stderr_ring))
            }
        }
    }

    /// run 引导退出后的移交核验：owner 就绪且身份匹配本工作区 →
    /// `Some(identity)`；预算内未出现匹配 owner（含初始化中/连接拒绝/
    /// legacy/身份不符）→ None。身份不符或 legacy 立即失败不重试——
    /// 重试只覆盖"尚未绑好"的过渡态。单次探测钳制到剩余预算（R4b）：
    /// 内部 HTTP 的自有超时不得越过观察预算。
    pub(super) async fn confirm_owner_handover(
        &self,
        workspace: &Path,
        budget: Duration,
    ) -> Option<shared_types::RuntimeIdentityView> {
        let deadline = std::time::Instant::now() + budget;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let address = self.config.app_cli_admin_probe_addr.clone();
            let probed = crate::service::dev_server::owner_client::probe_owner(&address);
            match tokio::time::timeout(remaining, probed).await {
                // 探测自身越过剩余预算：按预算耗尽收场（不报告移交成立）。
                Err(_) => return None,
                Ok(Ok(Some(identity))) => {
                    let application_id = std::env::var("PROJECT_ID")
                        .ok()
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or_else(|| "unknown-app".to_string());
                    if crate::service::dev_server::owner_client::verify_project_identity(
                        &identity,
                        workspace,
                        &application_id,
                    )
                    .is_ok()
                    {
                        return Some(identity);
                    }
                    tracing::warn!(
                        application_id,
                        "app-cli serve owner present after bootstrap but identity mismatch; refusing handover"
                    );
                    return None;
                }
                Ok(Ok(None)) => {
                    // 无人监听（含 legacy 应答）：引导没有留下可用的 serve。
                    // 竞态窗口内可能是尚未 bind——预算未尽则重试。
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    tokio::time::sleep(
                        HANDOVER_PROBE_RETRY
                            .min(deadline.saturating_duration_since(std::time::Instant::now())),
                    )
                    .await;
                }
                Ok(Err(_)) => {
                    // 初始化中/传输抖动：预算内重试，耗尽按未移交收场。
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    tokio::time::sleep(
                        HANDOVER_PROBE_RETRY
                            .min(deadline.saturating_duration_since(std::time::Instant::now())),
                    )
                    .await;
                }
            }
        }
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

/// A live management endpoint cannot prove that the original run succeeded.
/// Missing/unreadable exit status is an observation error, never exit-code zero.
fn require_successful_run_exit(
    exit: Option<crate::service::dev_server::supervise::ChildExit>,
    pid: u32,
    port: u16,
    stderr_ring: &Arc<StderrRing>,
) -> AppResult<()> {
    use crate::service::dev_server::supervise::ChildExit;
    match exit {
        Some(ChildExit::Exited(status)) if status.success() => Ok(()),
        Some(ChildExit::Exited(_)) => Err(early_exit_err(pid, port, stderr_ring)),
        Some(ChildExit::WaitFailed(error)) => Err(AppError::business(format!(
            "run handover exit status could not be observed: {error}; no successful handover confirmed"
        ))),
        None => Err(AppError::business(
            "run handover exit status is not yet confirmed; query the original launch before retrying",
        )),
    }
}

/// legacy app-cli 探测：/v1/deploy/status 是 app-cli 专属路由——200 且
/// 响应 data 信封内含 protocol_version 即为 app-cli 管理面（legacy 或
/// serve 形态）；foreign 服务 404/异构 body → false。
///
/// DEV-1 R2（app 211 事故根因之一）：真实 wire 是
/// `{"success":..,"code":"0000","data":{"protocol_version":4,..}}`——
/// protocol_version 嵌在 `data` 里；旧实现只查顶层导致活着的 run 被漏识别
/// （复用返回 Ok(None)、Legacy 分类失效）。无历史依据表明顶层裸值曾是
/// 真实契约，只支持已证实的 data 信封。
pub(crate) async fn legacy_app_cli_responds(address: &str) -> bool {
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
        .is_ok_and(|body| {
            body.get("data")
                .and_then(|data| data.get("protocol_version"))
                .is_some()
        })
}

#[cfg(test)]
mod exit_evidence_tests {
    use super::*;

    #[test]
    fn unobserved_or_unreadable_run_exit_cannot_confirm_handover() {
        let ring = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        for exit in [
            None,
            Some(
                crate::service::dev_server::supervise::ChildExit::WaitFailed(
                    "reaper failed".into(),
                ),
            ),
        ] {
            let error = require_successful_run_exit(exit, 1, 1, &ring)
                .expect_err("unknown exit status must not authorize a successful handover");
            assert!(error.to_string().contains("exit status"));
        }
    }
}

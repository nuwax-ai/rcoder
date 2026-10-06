use super::*;

pub(super) enum LaunchObservation {
    Ready,
    DeadlineExpired,
}

impl DevServerManager {
    /// 就绪轮询: 进程早退 → Err (读 stderr ring 分类成结构化错误); HTTP 就绪 → Ok;
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
        let max = self.config.dev_alive_max_wait_ms;
        let timeout = self.config.dev_alive_check_timeout_ms;
        let interval = Duration::from_millis(self.config.dev_alive_poll_interval_ms);
        // 用墙钟 deadline 计时 (而非固定步进累加): 探活本身可能耗时 (未 ready 时等到
        // timeout), 固定步进累加会让 max 超时判断严重偏松 (实际墙钟远大于累加值)。
        let deadline = std::time::Instant::now() + Duration::from_millis(max);
        loop {
            if launch_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                return Ok(LaunchObservation::DeadlineExpired);
            }
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
                return Ok(LaunchObservation::Ready);
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
        // 超时: 进程仍存活但未响应 HTTP → 按 nuwax 宽松返回成功; 若已死则分类报错
        if !process::is_process_running(pid) {
            return Err(early_exit_err(pid, port, stderr_ring));
        }
        tracing::warn!(
            "dev server on port {port} (pid {pid}) 未在 {max}ms 内响应 HTTP, 进程仍在 — 返回成功 (nuwax 宽松)"
        );
        Ok(LaunchObservation::Ready)
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

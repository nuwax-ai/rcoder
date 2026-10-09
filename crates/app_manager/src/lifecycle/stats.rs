//! UserApp 只读观测查询：资源用量（stats）、健康（health）、日志基址
//! （log base）、K8s 事件（events）。全部委托运行时/定位器，无生命周期副作用
//! （prod 日志基址的唤醒语义见 `log_api_base` 注释）。

use tracing::{instrument, warn};

use crate::models::*;
use crate::service::AppService;
use crate::utils::*;

impl AppService {
    /// 获取资源使用情况（app_stage 分派：prod=运行容器 label 查询；dev=开发容器
    /// 双键 selector——instance+service-type，K8s 专属）。
    ///
    /// CPU/内存用量 + 限额来自运行时（K8s = metrics.k8s.io PodMetrics + pod limits；Docker 默认 0），
    /// 百分比 = usage/limit×100（limit=0 → 0）。restart_count 来自 Deployment 状态
    /// （dev 形态为 STS 容器，restart 计数无对应视图 → 取 dev_container_alive 探活结果粗略映射 0/自身不计）。
    /// network（rx/tx）metrics.k8s.io 不提供，留 0。运行时用量查询失败降级为 0（不 500）。
    #[instrument(skip(self))]
    pub async fn get_app_stats(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
    ) -> AppResult<ResourceStats> {
        use shared_types::UserappStage;
        validate_app_id(app_id)?;
        if app_stage == UserappStage::Dev {
            return self.get_dev_stats(app_id).await;
        }
        let status = self.fetch_runtime_status_or_err(app_id).await?;
        let restart_count = status.restart_count;
        let usage = match self.runtime.get_app_resource_usage(app_id).await {
            Ok(u) => u,
            Err(e) => {
                warn!(
                    "[APP] get_app_resource_usage failed app_id={app_id}: {e} (stats fallback to zero)"
                );
                Default::default()
            }
        };
        Ok(Self::resource_stats_from(usage, restart_count))
    }

    /// 开发容器资源统计：`get_app_resource_usage_for(UserappBuilder)` 双键定位。
    /// 用量降级语义与 prod 相同；dev builder 常驻自愈，restart 视图不存在 → 0。
    async fn get_dev_stats(&self, app_id: &str) -> AppResult<ResourceStats> {
        let usage = match self
            .runtime
            .get_app_resource_usage_for(app_id, &shared_types::ServiceType::UserappBuilder)
            .await
        {
            Ok(u) => u,
            Err(e) => {
                warn!(
                    "[APP] dev resource usage failed app_id={app_id}: {e} (stats fallback to zero)"
                );
                Default::default()
            }
        };
        Ok(Self::resource_stats_from(usage, 0))
    }

    fn resource_stats_from(
        usage: container_runtime_api::ResourceUsage,
        restart_count: u32,
    ) -> ResourceStats {
        let cpu_percent = if usage.cpu_limit_cores > 0.0 {
            (usage.cpu_usage_cores / usage.cpu_limit_cores * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };
        let mem_percent = if usage.mem_limit_bytes > 0 {
            usage.mem_usage_bytes as f64 / usage.mem_limit_bytes as f64 * 100.0
        } else {
            0.0
        };
        ResourceStats {
            restart_count,
            cpu: CpuStats {
                usage_cores: usage.cpu_usage_cores,
                limit_cores: usage.cpu_limit_cores,
                usage_percent: cpu_percent,
            },
            memory: MemoryStats {
                usage_bytes: usage.mem_usage_bytes,
                limit_bytes: usage.mem_limit_bytes,
                usage_percent: mem_percent,
            },
            network: NetworkStats::default(),
        }
    }

    /// 获取应用健康状态（app_stage 分派）：
    /// - prod：实时集群查询派生（`AppRuntimeInfo.health`）
    /// - dev：探活开发容器内 file-server `/health`（经 `UserappDevLocator`
    ///   幂等 ensure+探活自愈定位）；2xx→Running / 其余→Unhealthy
    #[instrument(skip(self))]
    pub async fn get_app_health(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
    ) -> AppResult<HealthInfo> {
        validate_app_id(app_id)?;
        if app_stage == shared_types::UserappStage::Prod {
            let runtime = self.get_app(app_id).await?;
            return Ok(runtime.health);
        }
        // health 不在接口面收 user_id（⚪/dev🟢 不补参）——传 None 走 metadata
        // owner 链（ensure 侧取值链自降级，无需此处预查）
        let base = self.app_files_base(app_stage, app_id).await?;
        let ok = reqwest::Client::new()
            .get(format!("{base}/health"))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        Ok(HealthInfo {
            status: if ok { "Running" } else { "Unhealthy" }.to_string(),
            instance: None,
            probes: None,
        })
    }

    /// 日志转发基址。dev 只读定位已有 file-server，业务与 owner 未运行也可查。
    /// prod=唤醒后运行实例 IP
    /// （读日志是使用语义，闲置回收的 stopped 容器自动拉起——与文件族
    /// `app_files_base` prod 分支同款 wake）；dev=从
    /// `UserappDevLocator.dev_logs_file_server_addr`，不唤醒或创建开发容器。
    #[instrument(skip(self))]
    pub async fn log_api_base(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
    ) -> AppResult<String> {
        validate_app_id(app_id)?;
        if app_stage == shared_types::UserappStage::Prod {
            // 权威存在性检查前移：不存在的 app 直接 ERR_APP_NOT_FOUND。唤醒
            // 协调器现在会校验权威身份，对无记录 app 返回 Failed（runtime
            // 缺失），若先唤醒会把"应用不存在"吞成可重试的唤醒失败。
            self.get_app(app_id).await?;
            use shared_types::AppWakeControl;
            match self.activity.ensure_running(app_id).await {
                shared_types::WakeOutcome::Ready | shared_types::WakeOutcome::AlreadyRunning => {}
                shared_types::WakeOutcome::Timeout(detail) => {
                    return Err(AppOperationError::Diagnostic(detail));
                }
                shared_types::WakeOutcome::Blocked { message, blocker } => {
                    return Err(AppOperationError::ConflictBlocked { message, blocker });
                }
                shared_types::WakeOutcome::Failed(detail) => {
                    return Err(AppOperationError::Diagnostic(detail));
                }
            }
            let runtime = self.get_app(app_id).await?;
            let ip = runtime
                .health
                .instance
                .map(|instance| instance.ip)
                .filter(|ip| !ip.is_empty())
                .ok_or_else(|| {
                    AppOperationError::InvalidState(format!(
                        "app {app_id} has no ready runtime IP for log access"
                    ))
                })?;
            return Ok(format!("http://{ip}:{}", shared_types::APP_CLI_ADMIN_PORT));
        }
        let locator = self
            .dev_locator
            .read()
            .map_err(|error| AppOperationError::Backend(format!("read dev log locator: {error}")))?
            .clone()
            .ok_or_else(|| AppOperationError::Backend("dev log locator is unavailable".into()))?;
        locator
            .dev_logs_file_server_addr(app_id)
            .await
            .map_err(|error| {
                AppOperationError::Backend(format!(
                    "locate development logs for app {app_id}: {error}"
                ))
            })
    }

    /// 获取应用事件（K8s Events API：调度/拉取/启动/崩溃）
    #[instrument(skip(self))]
    pub async fn get_app_events(
        &self,
        app_id: &str,
    ) -> AppResult<Vec<container_runtime_api::AppEventInfo>> {
        validate_app_id(app_id)?;
        self.ensure_app_exists(app_id).await?;
        self.runtime.get_app_events(app_id).await.map_err(|e| {
            map_runtime_error(&format!("[APP] get_app_events failed app_id={app_id}"), e)
        })
    }
}

//! Userapp 生命周期 + 观测操作（从 service.rs 拆出，extension-impl）。
//!
//! start/stop/restart/recycle + stats/events 观测委托（转调 ContainerRuntime）。

use tracing::{debug, info, instrument, warn};

use container_runtime_api::DeploymentStatus;

use crate::models::*;
use crate::service::AppService;
use crate::utils::*;

impl AppService {
    /// 启动应用（scale replicas = 1）
    #[instrument(skip(self))]
    pub async fn start_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        let previous = self.fetch_runtime_status_or_err(app_id).await?;
        let previous_wake_on_traffic = previous
            .wake_on_traffic
            .unwrap_or_else(|| !self.activity.is_wake_blocked(app_id));
        self.runtime
            .patch_wake_on_traffic(app_id, true)
            .await
            .map_err(|error| {
                map_runtime_error(
                    &format!("[APP] enable wake-on-traffic failed app_id={app_id}"),
                    error,
                )
            })?;
        if let Err(error) = self.runtime.scale_deployment(app_id, 1).await {
            if let Err(restore_error) = self
                .runtime
                .patch_wake_on_traffic(app_id, previous_wake_on_traffic)
                .await
            {
                warn!(app_id, %restore_error, "failed to restore wake block after scale1 failure");
            }
            self.restore_activity_state(app_id, &previous, previous_wake_on_traffic);
            return Err(map_runtime_error(
                &format!("[APP] scale_deployment failed app_id={app_id}"),
                error,
            ));
        }
        self.activity.mark_running(app_id);
        info!("[APP] app started (scale=1): {}", app_id);
        self.get_app(app_id).await
    }

    /// 停止应用（scale replicas = 0）
    #[instrument(skip(self))]
    pub async fn stop_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        self.scale_to_zero(app_id, false).await
    }

    /// 闲置回收使用：scale0 后允许后续流量自动唤醒。
    #[instrument(skip(self))]
    pub async fn recycle_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        self.scale_to_zero(app_id, true).await
    }

    async fn scale_to_zero(
        &self,
        app_id: &str,
        wake_on_traffic: bool,
    ) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        let previous = self.fetch_runtime_status_or_err(app_id).await?;
        let previous_wake_on_traffic = previous
            .wake_on_traffic
            .unwrap_or_else(|| !self.activity.is_wake_blocked(app_id));
        // 先阻断内存态唤醒，再持久化停止原因，避免 scale0 与请求触发 scale1 竞态。
        self.activity.mark_wake_blocked(app_id);
        if let Err(error) = self
            .runtime
            .patch_wake_on_traffic(app_id, wake_on_traffic)
            .await
        {
            self.restore_activity_state(app_id, &previous, previous_wake_on_traffic);
            return Err(map_runtime_error(
                &format!("[APP] patch wake-on-traffic failed app_id={app_id}"),
                error,
            ));
        }
        if let Err(error) = self.runtime.scale_deployment(app_id, 0).await {
            self.restore_activity_state(app_id, &previous, previous_wake_on_traffic);
            if let Err(restore_error) = self
                .runtime
                .patch_wake_on_traffic(app_id, previous_wake_on_traffic)
                .await
            {
                warn!(app_id, %restore_error, "failed to restore wake-on-traffic after scale0 failure");
            }
            return Err(map_runtime_error(
                &format!("[APP] scale_deployment failed app_id={app_id}"),
                error,
            ));
        }
        if wake_on_traffic {
            self.activity.mark_recycled(app_id);
        }
        info!("[APP] app stopped (scale=0): {}", app_id);
        self.get_app(app_id).await
    }

    pub(crate) fn restore_activity_state(
        &self,
        app_id: &str,
        previous: &DeploymentStatus,
        previous_wake_on_traffic: bool,
    ) {
        if previous.replicas > 0 {
            self.activity.mark_running(app_id);
        } else if !previous_wake_on_traffic {
            self.activity.mark_wake_blocked(app_id);
        } else {
            self.activity.mark_recycled(app_id);
        }
    }

    /// 重启应用（rollout restart）
    #[instrument(skip(self))]
    pub async fn restart_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        self.ensure_app_exists(app_id).await?;
        self.runtime.restart_deployment(app_id).await.map_err(|e| {
            map_runtime_error(
                &format!("[APP] restart_deployment failed app_id={app_id}"),
                e,
            )
        })?;
        info!("[APP] app restarted (rollout): {}", app_id);
        self.get_app(app_id).await
    }

    /// 设置闲置回收策略（动态、免重启：strategic-merge Deployment 注解，不碰 pod template）。
    /// 供管理/运营面策略调整调用。Fail Fast：两字段皆 None → ERR_VALIDATION。
    #[instrument(skip(self))]
    pub async fn set_recycle_policy(
        &self,
        app_id: &str,
        request: RecyclePolicyRequest,
    ) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        // Fail Fast:先校验请求形状,再查 app 是否存在(空请求不浪费 K8s GET)
        validate_recycle_policy_fields(
            request.recycle_enabled,
            request.idle_timeout_seconds,
            request.wake_on_traffic,
        )?;
        self.ensure_app_exists(app_id).await?;
        self.runtime
            .patch_recycle_policy(
                app_id,
                request.recycle_enabled,
                request.idle_timeout_seconds,
            )
            .await
            .map_err(|e| {
                map_runtime_error(
                    &format!("[APP] patch_recycle_policy failed app_id={app_id}"),
                    e,
                )
            })?;
        // 流量唤醒语义独立注解（rcoder.io/wake-on-traffic），trait 默认 no-op
        //（无持久注解能力的 runtime 安全降级）；patch 失败上抛——注解写入是
        // 显式请求语义，静默丢弃会让 wake_on_traffic 回读与期望不一致。
        if let Some(enabled) = request.wake_on_traffic {
            self.runtime
                .patch_wake_on_traffic(app_id, enabled)
                .await
                .map_err(|e| {
                    // recycle 字段已生效（上方 patch 成功）：整体报错需注明部分
                    // 生效态，避免计费侧按"全部未生效"重放整组 patch。
                    map_runtime_error(
                        &format!(
                            "[APP] patch_wake_on_traffic failed app_id={app_id} \
                             (recycle_enabled/idle_timeout already applied)",
                        ),
                        e,
                    )
                })?;
        }
        info!(
            "[APP] recycle policy updated: {} (enabled={:?}, idle_timeout={:?}, wake_on_traffic={:?})",
            app_id, request.recycle_enabled, request.idle_timeout_seconds, request.wake_on_traffic
        );
        self.get_app(app_id).await
    }

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
        let base = self.app_files_base(app_stage, app_id, None).await?;
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

    /// 日志管理面转发基址（容器内 app-cli :3010）。prod=唤醒后运行实例 IP
    /// （读日志是使用语义，闲置回收的 stopped 容器自动拉起——与文件族
    /// `app_files_base` prod 分支同款 wake）；dev=从
    /// `UserappDevLocator.dev_file_server_addr`（:60000）解析 host 重拼端口
    /// （user_id 为 dev 懒创建容器的显式 owner 档）。
    #[instrument(skip(self))]
    pub async fn log_api_base(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
        user_id: &str,
    ) -> AppResult<String> {
        validate_app_id(app_id)?;
        if app_stage == shared_types::UserappStage::Prod {
            // 幻报拦截：ensure_running 对不存在的 app 返回 AlreadyRunning
            // （stopped-set 语义），get_app NotFound 兜底 404
            use shared_types::AppWakeControl;
            match self.activity.ensure_running(app_id).await {
                shared_types::WakeOutcome::Ready | shared_types::WakeOutcome::AlreadyRunning => {}
                shared_types::WakeOutcome::Timeout => {
                    return Err(AppOperationError::InvalidState(format!(
                        "app {app_id} wake timed out; retry later"
                    )));
                }
                shared_types::WakeOutcome::Failed(e) => {
                    return Err(AppOperationError::InvalidState(format!(
                        "app {app_id} wake failed: {e}"
                    )));
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
        let file_server = self
            .app_files_base(app_stage, app_id, Some(user_id))
            .await?;
        // dev 日志受理前置检查：app-cli 管理 API（:3010）随 dev 会话拉起/退出，
        // 会话不在时该端口为死端口——直连只会挂满连接超时（15s）后 500。
        // 未运行即刻 4xx 快速失败（ERR_DEV_NOT_RUNNING）。
        self.ensure_dev_logs_running(&file_server, app_id, user_id)
            .await?;
        // http://{host}:60000 → http://{host}:{APP_CLI_ADMIN_PORT}（host 段原样保留，仅换管理端口）
        let host = file_server
            .trim_start_matches("http://")
            .split(':')
            .next()
            .unwrap_or_default();
        Ok(format!(
            "http://{host}:{}",
            shared_types::APP_CLI_ADMIN_PORT
        ))
    }

    /// dev 日志可达性前置检查（`log_api_base` dev 分支调用）。
    ///
    /// 经容器内 file-server `/api/v1/userapp/dev/list` 判定——app-cli 即 dev
    /// 进程，在跑列表空 ⇔ :3010 死端口。仅"HTTP 200 且列表空"短路为
    /// `DevNotRunning`；预检自身 transport 失败/非 200/解码失败一律
    /// fail-open：预检不构成新故障面，照旧由后续 :3010 连接兜底
    /// （拿不准放行，对齐 model_probe 惯例）。
    async fn ensure_dev_logs_running(
        &self,
        file_server_base: &str,
        app_id: &str,
        user_id: &str,
    ) -> AppResult<()> {
        let response = reqwest::Client::new()
            .get(format!(
                "{file_server_base}/api/v1/userapp/dev/list?app_id={app_id}&user_id={user_id}"
            ))
            .timeout(DEV_LOGS_PRECHECK_TIMEOUT)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                debug!(app_id, %error, "dev logs precheck transport error, fail-open");
                return Ok(());
            }
        };
        if !response.status().is_success() {
            debug!(
                app_id,
                status = %response.status(),
                "dev logs precheck non-success, fail-open"
            );
            return Ok(());
        }
        match response.json::<DevListEnvelope>().await {
            // 仅"200 且 data.list 为空"短路；data 缺失（信封形态漂移）与
            // 解码失败同判 fail-open，不构成误报 DevNotRunning 的新故障面
            Ok(envelope) => match envelope.data {
                Some(data) if data.list.is_empty() => {
                    Err(AppOperationError::DevNotRunning(format!(
                        "app {app_id} development service is not running; start it before requesting development logs"
                    )))
                }
                _ => Ok(()),
            },
            Err(error) => {
                debug!(app_id, %error, "dev logs precheck decode error, fail-open");
                Ok(())
            }
        }
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

/// 校验 recycle-policy 请求至少带一个字段(纯函数,便于单测)。
fn validate_recycle_policy_fields(
    recycle_enabled: Option<bool>,
    idle_timeout_seconds: Option<u64>,
    wake_on_traffic: Option<bool>,
) -> AppResult<()> {
    if recycle_enabled.is_none() && idle_timeout_seconds.is_none() && wake_on_traffic.is_none() {
        return Err(AppOperationError::Validation(
            "recycle-policy requires at least one of recycle_enabled / idle_timeout_seconds / wake_on_traffic"
                .to_string(),
        ));
    }
    Ok(())
}

/// dev 日志前置检查（/dev/list）单请求超时——预检只许快，不得叠加等待
const DEV_LOGS_PRECHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// `/dev/list` 响应最小解析（HttpResult 信封的 `data.list`，仅判空——完整
/// 字段契约归 file-server-userapp `UserappDevList`，此处不重复建模）
#[derive(serde::Deserialize)]
struct DevListEnvelope {
    #[serde(default)]
    data: Option<DevListData>,
}

#[derive(serde::Deserialize)]
struct DevListData {
    #[serde(default)]
    list: Vec<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recycle_policy_requires_at_least_one_field() {
        // 三字段皆 None → Fail Fast
        assert!(validate_recycle_policy_fields(None, None, None).is_err());
        // 任一 Some → Ok
        assert!(validate_recycle_policy_fields(Some(true), None, None).is_ok());
        assert!(validate_recycle_policy_fields(Some(false), None, None).is_ok());
        assert!(validate_recycle_policy_fields(None, Some(60), None).is_ok());
        assert!(validate_recycle_policy_fields(None, None, Some(false)).is_ok());
        assert!(validate_recycle_policy_fields(Some(true), Some(60), Some(true)).is_ok());
    }
}

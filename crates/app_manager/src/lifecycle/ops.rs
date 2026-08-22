//! UserApp 生命周期 + 观测操作（从 service.rs 拆出，extension-impl）。
//!
//! start/stop/restart/recycle + stats/events 观测委托（转调 ContainerRuntime）。

use tracing::{info, instrument, warn};

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
    /// 供计费侧免费↔付费 tier 变更调用。Fail Fast：两字段皆 None → ERR_VALIDATION。
    #[instrument(skip(self))]
    pub async fn set_recycle_policy(
        &self,
        app_id: &str,
        request: RecyclePolicyRequest,
    ) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        // Fail Fast:先校验请求形状,再查 app 是否存在(空请求不浪费 K8s GET)
        validate_recycle_policy_fields(request.recycle_enabled, request.idle_timeout_seconds)?;
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
        info!(
            "[APP] recycle policy updated: {} (enabled={:?}, idle_timeout={:?})",
            app_id, request.recycle_enabled, request.idle_timeout_seconds
        );
        self.get_app(app_id).await
    }

    /// 获取资源使用情况。
    ///
    /// CPU/内存用量 + 限额来自运行时（K8s = metrics.k8s.io PodMetrics + pod limits；Docker 默认 0），
    /// 百分比 = usage/limit×100（limit=0 → 0）。restart_count 来自 Deployment 状态。
    /// network（rx/tx）metrics.k8s.io 不提供，留 0。运行时用量查询失败降级为 0（不 500）。
    #[instrument(skip(self))]
    pub async fn get_app_stats(&self, app_id: &str) -> AppResult<ResourceStats> {
        validate_app_id(app_id)?;
        let status = self.fetch_runtime_status_or_err(app_id).await?;
        let usage = match self.runtime.get_app_resource_usage(app_id).await {
            Ok(u) => u,
            Err(e) => {
                warn!("[APP] get_app_resource_usage failed app_id={app_id}: {e} (stats 降级 0)");
                Default::default()
            }
        };
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
        Ok(ResourceStats {
            restart_count: status.restart_count,
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

/// 校验 recycle-policy 请求至少带一个字段(纯函数,便于单测)。
fn validate_recycle_policy_fields(
    recycle_enabled: Option<bool>,
    idle_timeout_seconds: Option<u64>,
) -> AppResult<()> {
    if recycle_enabled.is_none() && idle_timeout_seconds.is_none() {
        return Err(AppOperationError::Validation(
            "recycle-policy requires at least one of recycle_enabled / idle_timeout_seconds"
                .to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recycle_policy_requires_at_least_one_field() {
        // 两字段皆 None → Fail Fast
        assert!(validate_recycle_policy_fields(None, None).is_err());
        // 任一 Some → Ok
        assert!(validate_recycle_policy_fields(Some(true), None).is_ok());
        assert!(validate_recycle_policy_fields(Some(false), None).is_ok());
        assert!(validate_recycle_policy_fields(None, Some(60)).is_ok());
        assert!(validate_recycle_policy_fields(Some(true), Some(60)).is_ok());
    }
}

//! UserApp 生命周期 + 观测操作（从 service.rs 拆出，extension-impl）。
//!
//! start/stop/restart + logs/stats/events/file_logs 观测委托（转调 ContainerRuntime）。

use tracing::{info, instrument, warn};

use super::models::*;
use super::service::AppService;
use super::utils::*;

impl AppService {
    /// 启动应用（scale replicas = 1）
    #[instrument(skip(self))]
    pub async fn start_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        self.ensure_app_exists(app_id).await?;
        self.runtime
            .scale_deployment(app_id, 1)
            .await
            .map_err(|e| {
                map_runtime_error(&format!("[APP] scale_deployment failed app_id={app_id}"), e)
            })?;
        self.activity.mark_running(app_id);
        info!("[APP] app started (scale=1): {}", app_id);
        self.get_app(app_id).await
    }

    /// 停止应用（scale replicas = 0）
    #[instrument(skip(self))]
    pub async fn stop_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        self.ensure_app_exists(app_id).await?;
        self.runtime
            .scale_deployment(app_id, 0)
            .await
            .map_err(|e| {
                map_runtime_error(&format!("[APP] scale_deployment failed app_id={app_id}"), e)
            })?;
        self.activity.mark_stopped(app_id);
        info!("[APP] app stopped (scale=0): {}", app_id);
        self.get_app(app_id).await
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

    /// 获取应用日志（实时拉容器 stdout/stderr：K8s Pod logs / docker logs）。
    ///
    /// `follow` 流式当前未实现（runtime 返回 tail 快照），`since` 暂未透传；
    /// SSE/WebSocket 实时流留待后续增强。
    #[instrument(skip(self))]
    pub async fn get_app_logs(&self, app_id: &str, params: LogParams) -> AppResult<Vec<LogEntry>> {
        validate_app_id(app_id)?;
        self.ensure_app_exists(app_id).await?;
        let tail = params.tail.unwrap_or(1000);
        let timestamps = params.timestamps.unwrap_or(true);
        let entries = self
            .runtime
            .get_app_logs(app_id, tail, timestamps)
            .await
            .map_err(|e| {
                map_runtime_error(&format!("[APP] get_app_logs failed app_id={app_id}"), e)
            })?;
        Ok(entries
            .into_iter()
            .map(|e| LogEntry {
                timestamp: e.timestamp.unwrap_or_default(),
                stream: e.stream,
                message: e.message,
            })
            .collect())
    }

    /// 启动日志流（follow），返回 mpsc::Receiver 供 WS handler 桥接（v2 §11）。
    /// receiver drop 即取消：客户端断开 → handler 退出 → receiver 析构 → runtime 任务终止。
    pub async fn stream_app_logs(
        &self,
        app_id: &str,
        tail: u32,
    ) -> AppResult<container_runtime_api::mpsc::Receiver<container_runtime_api::ContainerLogEntry>>
    {
        validate_app_id(app_id)?;
        self.runtime
            .stream_app_logs(app_id, tail)
            .await
            .map_err(|e| {
                map_runtime_error(&format!("[APP] stream_app_logs failed app_id={app_id}"), e)
            })
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

    /// 读取应用文件日志（从 workspace PVC 的 logs/ 目录直接读，不依赖 K8s Pod log API）。
    ///
    /// 适用：不写 stdout 而写文件的应用（Java Spring Boot → logs/application.log 等）。
    /// 路径相对 app 根（如 "logs/app.log"），有 path traversal 防护。
    #[instrument(skip(self))]
    pub async fn get_app_file_logs(
        &self,
        app_id: &str,
        file_path: &str,
        tail: u32,
    ) -> AppResult<Vec<LogEntry>> {
        validate_app_id(app_id)?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        let target = app_dir.join(file_path);

        // exists 守卫：日志文件不存在返 FileNotFound（常见，非 500）；canonicalize 失败也归此类
        if !target.exists() {
            return Err(AppOperationError::FileNotFound(format!(
                "log file does not exist: {file_path}"
            )));
        }
        // path traversal 防护（与 upload/delete_file 一致，复用 utils::ensure_within_app_dir）
        let canonical_root = app_dir.canonicalize().unwrap_or_else(|_| app_dir.clone());
        let canonical_target = ensure_within_app_dir(&target, &canonical_root)?;

        // 读文件，取最后 tail 行
        let content = tokio::fs::read_to_string(&canonical_target)
            .await
            .map_err(|e| {
                map_io_error(&format!("failed to read log file '{file_path}'"), e, true)
            })?;
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(tail as usize);
        Ok(lines[start..]
            .iter()
            .map(|line| LogEntry {
                timestamp: String::new(),
                stream: "file".to_string(),
                message: line.to_string(),
            })
            .collect())
    }
}

use super::*;

/// 部署期间的 liveness 端口托管（已退役——P1-01 修复后 API 先于 deploy_stage
/// 绑定，/health 恒200天然覆盖 kubelet liveness，不再需要独立占位）。
///
/// 保留结构定义作为设计参考。
#[allow(dead_code)]
pub struct LivenessHold {
    task: tokio::task::JoinHandle<()>,
}

impl LivenessHold {
    /// 绑定 admin 地址并开始应答探针。
    pub fn start(addr: &str) -> Result<Self> {
        let listener = std::net::TcpListener::bind(addr)
            .with_context(|| format!("bind liveness hold {addr}"))?;
        listener
            .set_nonblocking(true)
            .context("set liveness hold nonblocking")?;
        let listener = tokio::net::TcpListener::from_std(listener)
            .context("convert liveness hold listener")?;
        let task = tokio::spawn(async move {
            let app = axum::Router::new()
                .route(
                    "/health",
                    axum::routing::get(|| async {
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::json!({"status": "deploying"})),
                        )
                    }),
                )
                .route(
                    "/ready",
                    axum::routing::get(|| async {
                        (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            axum::Json(serde_json::json!({"status": "deploying"})),
                        )
                    }),
                )
                .fallback(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE });
            // 无限等待：task 被 abort（deploy 结束时）即整体退出
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { task })
    }

    /// 释放端口（abort serve task；listener 随 task 结束 drop）。
    pub async fn release(self) {
        self.task.abort();
        if let Err(e) = self.task.await
            && !e.is_cancelled()
        {
            warn!("liveness hold exited with error: {e}");
        }
    }
}

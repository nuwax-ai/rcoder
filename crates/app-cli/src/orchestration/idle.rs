//! 未部署 idle 常驻模式。
//!
//! 容器首次启动（rcoder start 无 url 创建空容器）时 workspace 无
//! `release.lock.toml`——api::serve 与 supervisor 都依赖 lock 起不来，
//! :3010 无人应答会被 kubelet liveness（部署链配的 :3010/health 探针）
//! 杀成 CrashLoop。本模块让 app-cli 以最小形态常驻：只应答探针，等待
//! 后续部署——`start{url}` 经 rcoder update 通道注入 `APP_DEPLOY_URL`
//! → config-hash 变更 → Recreate 整体换 Pod，本 Pod 直接被删，**不存在
//! 热切路径**（同进程重跑 supervisor 有 OnceLock/AppState 快照状态债）。
//!
//! 空容器阶段容器内 PG/ttyd/dbx 等由 supervisord 各固定 program 自治
//! 常驻，与 app-cli 无关——用户此时即可经平台代理连库建表、开终端。

use std::time::Duration;

/// idle 心跳日志间隔（静默分支留痕：进程活着且在等部署）。
const IDLE_LOG_INTERVAL: Duration = Duration::from_secs(300);

/// idle 常驻入口：绑定 admin addr 应答探针（/health 200、/ready 200、fallback 503），
/// 直到进程随容器终止（SIGTERM；或被部署动作 Recreate 换 Pod 整体替换）。
/// 无限循环，永不正常返回——无返回类型即此契约。
///
/// `/ready` 200 的语义：**空容器的基础设施就绪**（PG/ttyd/dbx 由 supervisord
/// 各 program 保证）——readiness 只表示"容器可服务"，应用层就绪在部署后才由
/// supervisor 的 set_ready 驱动（两层 ready 语义，勿混淆）。
///
/// bind 失败（端口被占等异常态）不 panic：warn 后裸 sleep——最坏 liveness
/// 失败由 kubelet 暴露问题，比 idle 退出进 FATAL 重试循环好排查。
pub async fn serve_forever(admin_addr: &str) {
    tracing::info!(
        "idle: no release.lock.toml found — entering idle mode (no app deployed; \
         PG/ttyd/dbx stay up via supervisord, waiting for start{{url}} deployment)"
    );
    if let Err(e) = hold_probes(admin_addr) {
        tracing::warn!("idle: bind {admin_addr} failed, sleeping without probe answers: {e:#}");
    } else {
        tracing::info!("idle: holding admin addr {admin_addr} (/health 200, /ready 200)");
    }
    loop {
        tokio::time::sleep(IDLE_LOG_INTERVAL).await;
        tracing::info!(
            "idle: no release deployed, awaiting start{{url}} deployment (pod will be replaced)"
        );
    }
}

/// 最小探针应答服务（idle 形态的 :3010 托管，结构对齐 [`crate::deploy::LivenessHold`]）。
/// serve task spawn 后 detach（tokio JoinHandle drop 不 abort），随进程终止退出。
fn hold_probes(addr: &str) -> anyhow::Result<()> {
    let listener = std::net::TcpListener::bind(addr)
        .map_err(|e| anyhow::anyhow!("bind idle hold {addr}: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| anyhow::anyhow!("set idle hold nonblocking: {e}"))?;
    let listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|e| anyhow::anyhow!("convert idle hold listener: {e}"))?;
    tokio::spawn(async move {
        // 无限等待：task 随进程终止（SIGTERM → runtime 关闭）退出
        let _ = axum::serve(listener, idle_router()).await;
    });
    Ok(())
}

/// idle 路由（独立函数便于直接测路由行为）。
fn idle_router() -> axum::Router {
    let status_idle = || async {
        (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({"status": "idle"})),
        )
    };
    axum::Router::new()
        .route("/health", axum::routing::get(status_idle))
        .route("/ready", axum::routing::get(status_idle))
        .fallback(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE })
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use tower::util::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn idle_router_answers_probes_and_rejects_rest() {
        let app = idle_router();

        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ready")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // 其余 API（logs/proxy 等）idle 态不可用 → 503
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/logs/query")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}

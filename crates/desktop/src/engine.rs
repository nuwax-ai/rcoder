//! desktop 的 rcoder 服务嵌入：自建 tokio runtime 上跑 `rcoder::run()`
//! 全量组合（引擎 + HTTP listener + Pingora），关窗发 SIGTERM 语义的
//! shutdown 由 OS 信号路径兜底（骨架阶段；完整优雅关停后续接
//! broadcast 通道）。运行时形态/健康探活经主端口 `/health`。

use std::time::Duration;

/// 主端口的健康探查（引擎状态卡用；deploy-host 默认 bind 127.0.0.1）。
pub async fn health_probe(port: u16) -> Result<(), String> {
    let url = format!("http://127.0.0.1:{port}/health");
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(&url)
        .timeout(Duration::from_secs(2))
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => Ok(()),
        Ok(r) => Err(format!("HTTP {}", r.status())),
        Err(e) => Err(e.to_string()),
    }
}

/// 在独立线程上启动完整 rcoder 服务（自建多线程 tokio runtime）。
///
/// 返回 join handle（骨架阶段不做窗口侧取消；服务随进程退出）。
pub fn spawn_rcoder_service() -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("rcoder-service".to_owned())
        .spawn(|| {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("[desktop] rcoder service runtime build failed: {error}");
                    return;
                }
            };
            if let Err(error) = runtime.block_on(rcoder::run()) {
                eprintln!("[desktop] rcoder service exited with error: {error:#}");
            }
        })
        .expect("spawn rcoder service thread")
}

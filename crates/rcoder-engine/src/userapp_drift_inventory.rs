//! UserApp 镜像漂移只读对账扫描器（spec sandbox-image-version-drift R3/R5）。
//!
//! 周期列出受管工作负载的模板镜像，与平台当前引用（prod=部署 env，
//! dev=builder 镜像配置）分类比对，输出结构化台账：对齐 / 平台内漂移
//! （同仓库不同 tag，冷启动会被 R1 对齐）/ 外部或钉住（保留）/ 目标缺失
//! （部署缺配置）。**只读**：不做任何收敛，收敛只发生在启动/唤醒边界。

use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

use crate::app_state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriftClass {
    /// 与平台当前引用一致。
    Aligned,
    /// 同仓库不同 tag：平台管理的旧版本，冷启动会被启动对齐收敛（R1）。
    DriftedPlatform,
    /// 不同仓库：外部或显式钉住的镜像，启动对齐会保留（归属守卫）。
    Foreign,
    /// 平台当前引用缺失（部署 env/镜像配置缺），无法判定。
    UnknownTarget,
}

/// 纯分类：按仓库段判定归属，与启动对齐守卫（container-runtime-api
/// `image_repository`）同一语义。
pub(crate) fn classify_drift(image: &str, target: Option<&str>) -> DriftClass {
    let Some(target) = target else {
        return DriftClass::UnknownTarget;
    };
    if image == target {
        return DriftClass::Aligned;
    }
    if container_runtime_api::image_repository(image)
        == container_runtime_api::image_repository(target)
    {
        DriftClass::DriftedPlatform
    } else {
        DriftClass::Foreign
    }
}

pub(crate) struct DriftInventoryRuntimeConfig {
    pub scan_interval: Duration,
}

pub(crate) struct DriftInventoryScanner {
    config: DriftInventoryRuntimeConfig,
    state: Arc<AppState>,
}

impl DriftInventoryScanner {
    pub(crate) fn new(config: DriftInventoryRuntimeConfig, state: Arc<AppState>) -> Self {
        Self { config, state }
    }

    pub(crate) async fn run(self, mut shutdown_rx: tokio::sync::broadcast::Receiver<()>) {
        let mut interval = tokio::time::interval(self.config.scan_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await; // 消耗首次立即 tick（启动 grace）
        info!(
            interval = ?self.config.scan_interval,
            "[IMAGE_DRIFT_INVENTORY] scanner started"
        );
        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    info!("[IMAGE_DRIFT_INVENTORY] scanner stopped");
                    return;
                }
                _ = interval.tick() => {
                    if let Err(error) = self.do_scan().await {
                        warn!(%error, "[IMAGE_DRIFT_INVENTORY] scan failed");
                    }
                }
            }
        }
    }

    async fn do_scan(&self) -> anyhow::Result<()> {
        let workloads = self
            .state
            .runtime()
            .list_userapp_workload_images()
            .await
            .map_err(|error| anyhow::anyhow!("list userapp workloads: {error}"))?;
        if workloads.is_empty() {
            info!("[IMAGE_DRIFT_INVENTORY] no managed userapp workloads");
            return Ok(());
        }
        let prod_target = std::env::var("RCODER_RUNTIME_IMAGE_DIGEST")
            .ok()
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let dev_target = self
            .state
            .runtime()
            .current_builder_image()
            .await
            .map_err(|error| anyhow::anyhow!("resolve current builder image: {error}"))?;
        let mut counts = [0usize; 4];
        for workload in &workloads {
            let target = match workload.stage.as_str() {
                "prod" => prod_target.as_deref(),
                "dev" => dev_target.as_deref(),
                _ => None,
            };
            let class = classify_drift(&workload.image, target);
            counts[class as usize] += 1;
            match class {
                DriftClass::DriftedPlatform => warn!(
                    app = %workload.app_id,
                    stage = %workload.stage,
                    workload = %workload.workload_name,
                    image = %workload.image,
                    platform = target.unwrap_or_default(),
                    "[IMAGE_DRIFT_INVENTORY] platform-managed stale image (aligns on next cold start per R1)"
                ),
                DriftClass::Foreign => info!(
                    app = %workload.app_id,
                    stage = %workload.stage,
                    workload = %workload.workload_name,
                    image = %workload.image,
                    "[IMAGE_DRIFT_INVENTORY] foreign or pinned image retained"
                ),
                DriftClass::UnknownTarget => warn!(
                    app = %workload.app_id,
                    stage = %workload.stage,
                    "[IMAGE_DRIFT_INVENTORY] platform image reference missing; drift not classified"
                ),
                DriftClass::Aligned => {}
            }
        }
        info!(
            total = workloads.len(),
            aligned = counts[DriftClass::Aligned as usize],
            drifted_platform = counts[DriftClass::DriftedPlatform as usize],
            foreign_or_pinned = counts[DriftClass::Foreign as usize],
            unknown_target = counts[DriftClass::UnknownTarget as usize],
            "[IMAGE_DRIFT_INVENTORY] scan complete"
        );
        Ok(())
    }
}

pub(crate) fn start_drift_inventory_task(
    config: DriftInventoryRuntimeConfig,
    state: Arc<AppState>,
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
) -> tokio::task::JoinHandle<()> {
    let scanner = DriftInventoryScanner::new(config, state);
    tokio::task::spawn(scanner.run(shutdown_tx.subscribe()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_matches_alignment_guard_semantics() {
        use DriftClass::*;
        assert_eq!(
            classify_drift("reg/ns/app:1", Some("reg/ns/app:1")),
            Aligned
        );
        assert_eq!(
            classify_drift("reg/ns/app:0.1.306", Some("reg/ns/app:0.1.456")),
            DriftedPlatform
        );
        assert_eq!(
            classify_drift("docker.io/someone/custom:1", Some("reg/ns/app:1")),
            Foreign
        );
        assert_eq!(classify_drift("sha256:abc", Some("reg/ns/app:1")), Foreign);
        assert_eq!(classify_drift("reg/ns/app:1", None), UnknownTarget);
    }
}

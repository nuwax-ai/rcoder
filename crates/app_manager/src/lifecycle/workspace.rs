//! UserApp workspace PVC 就绪（从 service.rs 拆出，extension-impl）。
//!
//! RBD 卷形态（rcoder 零挂载）：本模块只剩 PVC ensure——rcoder 不再解析卷路径、
//! 不再建目录（code/data/logs 由 app-runtime 镜像 + app-cli 部署段在容器内就位），
//! Docker bind 源目录由 docker runtime 在 create_deployment 前建。

use std::path::PathBuf;

use shared_types::ServiceType;

use crate::models::*;
use crate::service::AppService;

impl AppService {
    /// ensure UserApp per-app 工作空间就绪 (K8s): ensure PVC 带 requests.storage 用户配额。
    ///
    /// RBD 卷无 `subvolumePath`（CephFS CSI 专有字段），故**不等待 resolve**——PVC
    /// 绑定由首个 Pod 调度触发（WaitForFirstConsumer），rcoder 无需卷路径视角。
    /// 必须在 create_deployment 之前调用：首次 ensure 带配额，否则 create 内 ensure
    /// 命中 active 复用会丢配额。Docker: 无 per-app PVC → no-op。
    pub(crate) async fn ensure_app_workspace_ready(
        &self,
        app_id: &str,
        storage_size: Option<&str>,
    ) -> AppResult<()> {
        if !shared_types::is_kubernetes_runtime() {
            return Ok(()); // Docker 无 per-app PVC
        }
        self.runtime
            .ensure_workspace(app_id, &ServiceType::UserApp, storage_size)
            .await
            .map_err(|e| {
                AppOperationError::Backend(format!("ensure UserApp PVC (app_id={app_id}): {e}"))
            })
    }

    /// Docker 模式 bind mount 宿主源路径（K8s per-app PVC 不用此值）。
    ///
    /// `workspace_root/{app_id}` 经 HostPathResolver 反解为宿主路径（rcoder 通常也
    /// 运行在容器内）；解析失败回退原路径。
    pub(crate) fn get_host_app_dir(&self, app_id: &str) -> PathBuf {
        let p = PathBuf::from(self.config.get_workspace_root()).join(app_id);
        if let Some(resolver) = self.path_resolver.get("default") {
            resolver.resolve_to_host_path(&p).unwrap_or(p)
        } else {
            p
        }
    }
}

//! UserApp workspace PVC 就绪（从 service.rs 拆出，extension-impl）。
//!
//! RBD 卷形态（rcoder 零挂载）：本模块只剩 PVC ensure——rcoder 不再解析卷路径、
//! 不再建目录（prod 四目录由 runtime 在 create_deployment 前经 userapp-workspace
//! 锚点预创建 + 容器内挂载就位）。

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
}

//! 应用管理处理器（facade——按接口组拆分到子模块）
//!
//! 子模块划分：
//! - [`state`]：处理器共享状态 [`AppManagerState`]
//! - [`lifecycle`]：create / query / get / update / delete
//! - [`ops`]：start / stop / restart
//! - [`query`]：health / stats / events
//! - [`files`]：upload / list / delete
//! - [`storage`]：get / clear / destroy / query（v2 §5.4）
//!
//! 数据库管理原属 [`db`] 子模块（`{app_id}/db/*`），已按拍板下线——统一走
//! rcoder 转发层的 `/api/v1/userapp/db/{env}/*`（env 双环境 + username upsert
//! + dbx 同步的超集实现，见 rcoder userapp_forward::db）。

pub mod files;
pub mod lifecycle;
pub mod logs;
pub mod ops;
pub mod query;
pub mod state;
pub mod storage;

pub use files::*;
pub use lifecycle::*;
pub use logs::*;
pub use ops::*;
pub use query::*;
pub use state::AppManagerState;
pub use storage::*;

// health 信息已合并到 AppRuntimeInfo.health（由 build_runtime_info 经 health_from_status 统一派生）；
// get_app_health 直接取 runtime.health，无需 handler 重复派生（消除 m1 重复）。

/// app 操作错误 → HTTP 响应错误（v2 §12）。
///
/// service 层返回强类型 [`AppOperationError`]（variant 携带错误码），handler 通过 From
/// 直接转换——错误码在 service 抛出点确定（Fail Fast），无需 downcast / 字符串匹配。
impl From<crate::error::AppOperationError> for shared_types::AppError {
    fn from(e: crate::error::AppOperationError) -> Self {
        shared_types::AppError::with_message(e.code(), e.message().to_string())
    }
}

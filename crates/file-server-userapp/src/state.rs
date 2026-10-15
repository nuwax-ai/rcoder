//! userApp 域共享状态。

use std::sync::Arc;

use crate::service::userapp::tasks::BuildTaskStore;

/// userApp 路由的 axum state：file-server 共享单例设施经 `fs` 引用（同进程
/// 零开销——dev_server 进程表 / log_cache / build_manager / config 与 TS 对齐
/// 路由共用），编译任务表为本域自有。
#[derive(Clone)]
pub struct UserAppState {
    pub fs: file_server::AppState,
    pub build_tasks: Arc<BuildTaskStore>,
}

impl UserAppState {
    /// 组装时构造（任务表新建——BuildTaskStore 纯内存、无持久化）。
    pub fn new(fs: file_server::AppState) -> Self {
        Self {
            fs,
            build_tasks: Arc::new(BuildTaskStore::new()),
        }
    }
}

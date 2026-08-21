//! UserApp 开发资源清理契约（跨 crate，按模块契约约定置于 shared_types）。

use async_trait::async_trait;

/// UserApp 开发资源回收（per-app 开发容器 + 开发 PVC）。
///
/// app_manager 的 runtime 视图（`UserAppRuntime`）经 ISP 分层不含 agent 容器能力，
/// 但 app 删除（purge）需要回收 UserAppBuilder 开发容器与 per-app RWO PVC——
/// 经此契约回调到宿主（rcoder，持有 `ContainerRuntime` 全量视图）执行。
///
/// 实现要求：幂等（资源不存在视为成功）；失败语义由调用方决定
/// （purge 路径 best-effort：失败 warn 不阻断 app 删除，下次 purge 重试收敛）。
#[async_trait]
pub trait UserappDevCleanup: Send + Sync {
    async fn cleanup(&self, app_id: &str) -> Result<(), String>;
}

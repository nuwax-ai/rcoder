//! `UserappDevLocator` 契约实现：app_manager 文件/存储八接口 `env=dev` 分支的
//! 开发容器定位回调（复用注册表 ensure + 探活自愈 + file-server 地址解析）。

use std::sync::{Arc, Weak};

use shared_types::ServiceType;

use super::{dev_file_server_addr, ensure_userapp_builder_probed};
use crate::router::AppState;

/// Weak 挂接 [`AppState`]——注入发生在 `AppState` Arc 包装后；Weak 防与
/// `AppState → app_service → dev_locator → AppState` 的引用环。
pub struct UserappDevLocator {
    state: Weak<AppState>,
}

impl UserappDevLocator {
    pub(crate) fn new(state: Weak<AppState>) -> Self {
        Self { state }
    }

    fn state(&self) -> Result<Arc<AppState>, String> {
        self.state
            .upgrade()
            .ok_or_else(|| "app state already dropped".to_string())
    }
}

#[async_trait::async_trait]
impl shared_types::UserappDevLocator for UserappDevLocator {
    async fn dev_file_server_addr(
        &self,
        app_id: &str,
        user_id: Option<&str>,
    ) -> Result<String, String> {
        let state = self.state()?;
        // 低频管理面语义：先探活再返回（注册表命中死容器时自愈重建），
        // 与 pod ensure/keepalive 同款；热路径转发层不走这里（自有 30s 探活缓存）。
        let (info, created) = ensure_userapp_builder_probed(&state, app_id, user_id)
            .await
            .map_err(|e| format!("ensure UserAppBuilder (app {app_id}): {e:#}"))?;
        if created {
            tracing::info!("[USERAPP_DEV_LOCATOR] builder ensured on demand: app_id={app_id}");
        }
        Ok(dev_file_server_addr(&state, &info))
    }

    async fn dev_container_alive(&self, app_id: &str) -> Result<bool, String> {
        let state = self.state()?;
        state
            .runtime()
            .find_container(app_id, &ServiceType::UserAppBuilder)
            .await
            .map(|found| found.is_some())
            .map_err(|e| format!("find UserAppBuilder (app {app_id}): {e}"))
    }
}

/// 终端代理（rcoder-proxy）的 dev 容器懒启动回调：容器不在时自动 ensure
/// 创建（owner 走 metadata 链——浏览器终端 URL 无入参携带能力）。
#[async_trait::async_trait]
impl shared_types::UserappDevEnsure for UserappDevLocator {
    async fn ensure_dev_container(
        &self,
        app_id: &str,
        user_id: Option<&str>,
    ) -> Result<shared_types::ContainerBasicInfo, String> {
        let state = self.state()?;
        let (info, created) = ensure_userapp_builder_probed(&state, app_id, user_id)
            .await
            .map_err(|e| format!("ensure UserAppBuilder (app {app_id}): {e:#}"))?;
        if created {
            tracing::info!("[USERAPP_DEV_LOCATOR] builder ensured on demand: app_id={app_id}");
        }
        Ok(info)
    }
}

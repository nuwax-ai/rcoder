//! Per-agent-version 安装并发控制
//!
//! 防止同一 agent 同一版本的重复安装，支持强制重装（取消正在进行的安装）。
//! 允许同一 agent 的不同版本并发安装。

use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// 单个 agent 的安装状态
pub struct InstallState {
    /// 安装互斥锁（per-agent-id 串行化）
    lock: Mutex<()>,
    /// 当前正在安装的版本（None = 未在安装）
    installing_version: parking_lot::Mutex<Option<String>>,
    /// 取消令牌（force=true 时取消正在进行的下载）
    ///
    /// 使用 Mutex 包装，因为 CancellationToken 不支持 reset，
    /// force-cancel 后需要替换为新的 token。
    cancel: parking_lot::Mutex<CancellationToken>,
}

impl InstallState {
    /// 尝试获取安装锁（非阻塞）
    ///
    /// 成功返回 `Some(MutexGuard)`，失败返回 `None`（正在安装中）。
    pub fn try_lock(&self) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        self.lock.try_lock().ok()
    }

    /// 获取安装锁（阻塞等待）
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.lock.lock().await
    }

    /// 标记开始安装指定版本
    pub fn set_installing(&self, version: &str) {
        if let Some(mut guard) = self.installing_version.try_lock() {
            *guard = Some(version.to_string());
        }
    }

    /// 清除安装状态
    pub fn clear_installing(&self) {
        if let Some(mut guard) = self.installing_version.try_lock() {
            *guard = None;
        }
    }

    /// 获取当前正在安装的版本
    pub fn installing_version(&self) -> Option<String> {
        self.installing_version.try_lock().and_then(|g| g.clone())
    }

    /// 取消当前正在进行的安装
    ///
    /// 使用 try_lock 避免阻塞 tokio worker 线程。
    /// 如果锁被短暂持有（赋值操作），try_lock 几乎总是成功；
    /// 极端竞争下会跳过本次取消（调用方可重试）。
    pub fn cancel(&self) {
        if let Some(guard) = self.cancel.try_lock() {
            guard.cancel();
        }
    }

    /// 替换取消令牌（安装开始时调用，确保新安装使用未取消的 token）
    pub fn replace_cancel_token(&self, new_token: CancellationToken) {
        if let Some(mut guard) = self.cancel.try_lock() {
            *guard = new_token;
        }
    }

    /// 获取当前取消令牌的快照（仅供测试）
    #[cfg(test)]
    fn cancel_token_snapshot(&self) -> CancellationToken {
        self.cancel.lock().clone()
    }
}

/// 全局安装锁管理器
///
/// 持有 `Arc<InstallLockManager>` 在 `AgentMgmtHttpState` 中，
/// 所有 install 端点共享同一实例。
///
/// 锁键为 `"{agent_id}:{version}"`，允许同一 agent 的不同版本并发安装。
pub struct InstallLockManager {
    states: DashMap<String, Arc<InstallState>>,
}

impl InstallLockManager {
    pub fn new() -> Self {
        Self {
            states: DashMap::new(),
        }
    }

    /// 构造锁键："{agent_id}:{normalized_version}"
    ///
    /// 版本号必须是合法 semver，否则返回 `VersionParseError`。
    fn lock_key(
        agent_id: &str,
        version: &str,
    ) -> Result<String, shared_types::version_util::VersionParseError> {
        let normalized = shared_types::version_util::normalize_version(version)?;
        Ok(format!("{}:{}", agent_id, normalized))
    }

    /// 获取指定 agent 版本的安装状态（不存在则创建）
    ///
    /// 版本号非法时返回 `None`。
    pub fn get_or_create(&self, agent_id: &str, version: &str) -> Option<Arc<InstallState>> {
        let key = Self::lock_key(agent_id, version).ok()?;
        Some(
            self.states
                .entry(key)
                .or_insert_with(|| {
                    Arc::new(InstallState {
                        lock: Mutex::new(()),
                        installing_version: parking_lot::Mutex::new(None),
                        cancel: parking_lot::Mutex::new(CancellationToken::new()),
                    })
                })
                .clone(),
        )
    }

    /// 检查指定 agent 版本是否正在安装（仅供测试使用）
    #[cfg(test)]
    pub fn is_installing(&self, agent_id: &str, version: &str) -> Option<String> {
        let key = Self::lock_key(agent_id, version).ok()?;
        self.states.get(&key).and_then(|s| s.installing_version())
    }
}

impl Default for InstallLockManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_manager_has_no_installing() {
        let mgr = InstallLockManager::new();
        assert!(mgr.is_installing("agent-x", "1.0.0").is_none());
    }

    #[test]
    fn get_or_create_returns_same_state() {
        let mgr = InstallLockManager::new();
        let s1 = mgr.get_or_create("agent-x", "1.0.0").unwrap();
        let s2 = mgr.get_or_create("agent-x", "1.0.0").unwrap();
        assert!(Arc::ptr_eq(&s1, &s2));
    }

    #[test]
    fn different_versions_get_different_states() {
        let mgr = InstallLockManager::new();
        let s1 = mgr.get_or_create("agent-x", "1.0.0").unwrap();
        let s2 = mgr.get_or_create("agent-x", "2.0.0").unwrap();
        assert!(!Arc::ptr_eq(&s1, &s2));
    }

    #[test]
    fn different_agents_get_different_states() {
        let mgr = InstallLockManager::new();
        let s1 = mgr.get_or_create("agent-a", "1.0.0").unwrap();
        let s2 = mgr.get_or_create("agent-b", "1.0.0").unwrap();
        assert!(!Arc::ptr_eq(&s1, &s2));
    }

    #[test]
    fn set_and_clear_installing() {
        let mgr = InstallLockManager::new();
        let state = mgr.get_or_create("agent-x", "1.0.0").unwrap();

        state.set_installing("1.0.0");
        assert_eq!(
            mgr.is_installing("agent-x", "1.0.0"),
            Some("1.0.0".to_string())
        );

        state.clear_installing();
        assert!(mgr.is_installing("agent-x", "1.0.0").is_none());
    }

    #[tokio::test]
    async fn try_lock_succeeds_when_free() {
        let mgr = InstallLockManager::new();
        let state = mgr.get_or_create("agent-x", "1.0.0").unwrap();
        assert!(state.try_lock().is_some());
    }

    #[tokio::test]
    async fn try_lock_fails_when_held() {
        let mgr = InstallLockManager::new();
        let state = mgr.get_or_create("agent-x", "1.0.0").unwrap();
        let _guard = state.try_lock().unwrap();
        assert!(state.try_lock().is_none());
    }

    #[tokio::test]
    async fn cancel_does_not_affect_other_versions() {
        let mgr = InstallLockManager::new();
        let s1 = mgr.get_or_create("agent-x", "1.0.0").unwrap();
        let s2 = mgr.get_or_create("agent-x", "2.0.0").unwrap();

        // s2 的初始 token 不受影响
        let token_before = s2.cancel_token_snapshot();
        s1.cancel();
        assert!(!token_before.is_cancelled());
    }
}

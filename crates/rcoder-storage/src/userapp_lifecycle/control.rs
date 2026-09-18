//! 存储生命周期控制（specs/userapp-turso-local-storage trait-design §6）。
//!
//! 业务服务只持有 [`UserAppLifecycleStore`]；只有启动/关机装配层获得
//! [`UserAppStoreControl`]。不为 worker 后端给所有 handler 添加 shutdown
//! 能力，也不把关闭入口开放给普通业务消费者。

use shared_types::UserAppStoreError;

/// 关机控制：停止接单 → 完成已接收事务 → 关闭连接/池 → 释放独占锁。
///
/// - 重复调用安全（共享同一关闭结果，不重复执行清理）。
/// - 并发入队与关闭有明确边界：关闭后遗留 store 引用的调用返回错误，
///   不挂起、不返回假成功。
/// - Drop 只作兜底，不充当已完成 flush 的证据。
#[async_trait::async_trait]
pub trait UserAppStoreControl: Send + Sync {
    async fn shutdown(&self) -> Result<(), UserAppStoreError>;
}

/// 配置工厂的装配结果：`store` 注入业务层，`control` 留给关机协调者。
pub struct OpenedUserAppStore {
    pub store: std::sync::Arc<dyn shared_types::UserAppLifecycleStore>,
    pub control: std::sync::Arc<dyn UserAppStoreControl>,
}

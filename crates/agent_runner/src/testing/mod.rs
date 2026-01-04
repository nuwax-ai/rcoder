//! 测试辅助模块
//!
//! 仅在 `testing` feature 启用时编译

// 阻塞注入模块 (用于极端场景测试)
#[cfg(feature = "test-blocking")]
pub mod blocking;

#[cfg(feature = "test-blocking")]
pub use blocking::{BlockingConfig, inject_blocking};

// 测试 Fixtures (通用测试辅助工具)
pub mod fixtures;
pub use fixtures::TestRequestBuilder;

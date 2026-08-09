//! 进程级共享 HTTP 客户端工厂 (消灭裸 `reqwest::Client::new()`)。
//!
//! 约定:
//! - 客户端只设 **连接超时** (15s); 不设总超时 —— 该 client 同时服务 SSE 长连接
//!   (build 日志流/会话事件流可持续数分钟), 总超时会误杀;
//! - 一次性短请求需要总超时的, 调用方在 `RequestBuilder` 上 `.timeout(...)` 单独设置;
//! - `reqwest::Client` 内部持有连接池且 Clone 廉价 (Arc), 全进程复用同一实例。

use std::sync::LazyLock;
use std::time::Duration;

/// 共享客户端连接建立超时 (秒)。
pub const SHARED_CONNECT_TIMEOUT_SECS: u64 = 15;

static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(SHARED_CONNECT_TIMEOUT_SECS))
        .build()
        // builder 失败仅可能在 TLS 后端初始化异常时发生; 退化为无超时 client 保持可用
        .unwrap_or_else(|_| reqwest::Client::new())
});

/// 获取进程级共享 HTTP 客户端。
pub fn shared_client() -> &'static reqwest::Client {
    &HTTP_CLIENT
}

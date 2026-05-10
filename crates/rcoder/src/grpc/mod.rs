//! gRPC 客户端模块
//!
//! 提供 rcoder 与 agent_runner 之间的 gRPC 通信客户端实现

pub mod channel_pool;
pub mod chat_client;
pub mod converters;
pub mod error;
pub mod locale_metadata;
pub mod sse_stream;

pub use channel_pool::GrpcChannelPool;
pub use chat_client::*;
pub use converters::*;
pub use error::*;
pub use locale_metadata::*;
pub use sse_stream::*;

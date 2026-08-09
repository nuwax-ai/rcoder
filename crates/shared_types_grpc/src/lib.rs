//! gRPC 类型定义
//!
//! 从 shared_types 拆分出来的 gRPC 相关类型，包含：
//! - protobuf 生成的消息类型和服务定义
//! - MaskedModelConfig 脱敏包装器
//! - URL 脱敏工具
//!
//! 此 crate 的目的是将 tonic/prost 等 gRPC 依赖从 shared_types 中移除，
//! 让只需要数据类型的 crate 不必引入 gRPC 依赖。

/// gRPC 生成的类型（protobuf 消息和服务）
pub mod grpc {
    // prost-build 生成代码: 输出风格固定使用全限定路径 (::prost::alloc::string::String 等),
    // 不属于手写代码质量问题, 抑制相应 lint。
    #![allow(unused_qualifications)]
    include!("grpc/agent.rs");
}

/// URL 脱敏工具
mod grpc_mask;
pub use grpc_mask::mask_url;

/// gRPC 脱敏包装器
mod grpc_wrapper;
pub use grpc_wrapper::MaskedModelConfig;

// 重新导出常用的 gRPC 类型，方便下游使用
pub use grpc::*;

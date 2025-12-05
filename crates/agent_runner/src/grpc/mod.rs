//! gRPC 服务模块
//!
//! 提供 agent_runner 的 gRPC 服务端实现，用于替代原有的 HTTP 接口

pub mod agent_service_impl;

pub use agent_service_impl::AgentServiceImpl;

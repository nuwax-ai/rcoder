//! rcoder-gateway: K8s 模式下的无状态网关
//!
//! 职责：
//! 1. 匹配路由（数据面 vs 控制面）
//! 2. 数据面：从 body/path 提取 identifier → 注入 x-backend-cluster header → 转发 Envoy Gateway
//! 3. 控制面：透传到 rcoder-control
//!
//! 不做：不管理 Pod IP、不做健康检查、不做连接池

pub mod cluster_cache;
pub mod config;
pub mod control_plane_client;
pub mod gateway_proxy;
pub mod gateway_service;
pub mod identifier_extractor;
pub mod route_table;
pub mod session_resolver;

pub use config::GatewayConfig;
pub use gateway_proxy::GatewayProxy;
pub use gateway_service::GatewayService;

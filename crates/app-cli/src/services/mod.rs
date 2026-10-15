//! 服务承载域：supervisor 编排核心、supervisord 引擎适配（XML-RPC）、服务规格
//! 与执行入口、type=static 静态托管和工作区索引服务。
pub mod run_service;
pub mod static_hosting;
pub mod supervisor;
pub mod supervisord_host;
pub mod svc_spec;
pub mod workspace_index;
pub mod xmlrpc;

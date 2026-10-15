//! Agent HTTP 契约域 —— rcoder ↔ agent_runner 的 HTTP API 类型 / 服务 trait / 通用 handler / 安装管理
//!
//! - `types`：Agent HTTP API 类型（rcoder 和 agent_runner 共用）
//! - `computer_types`：Computer Agent HTTP API 类型
//! - `rcoder_types`：RCoder Agent HTTP API 类型
//! - `http_service`：[`AgentHttpService`] trait（三种后端的统一抽象）
//! - `http_handlers`：通用 HTTP Handlers（基于 trait；模块名经 lib.rs 根部 re-export 保留）
//! - `mgmt_types`：Agent 二进制安装/管理类型（agent_runner 的 agent_mgmt 消费）
//! - `chat_config`：Chat Agent 配置（模型环境绑定 / 工具审批规则等）
//!
//! 对外统一经 crate 根部 re-export 暴露（如 `shared_types::AgentHttpService`），
//! 下游不应依赖 `shared_types::agent::` 路径。
//!
//! [`AgentHttpService`]: http_service::AgentHttpService

pub mod chat_config;
pub mod computer_types;
pub mod http_handlers;
pub mod http_service;
pub mod mgmt_types;
pub mod rcoder_types;
pub mod types;

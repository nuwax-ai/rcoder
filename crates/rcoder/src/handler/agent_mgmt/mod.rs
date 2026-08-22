//! Agent Management HTTP 处理器 (P0-4 + P0-5 重构)
//!
//! 提供 7 个 HTTP 端点(与 agent-runner 端契约一致),内部通过 gRPC 转发到
//! 对应项目的 agent_runner 容器:
//!
//! - `POST /agent-mgmt/agents/list?project_id=xxx`           list_agents (body JSON)
//! - `POST /agent-mgmt/agents/get?project_id=xxx`            get_agent    (body JSON)
//! - `POST /agent-mgmt/agents/check?project_id=xxx`          check_agent  (body JSON)
//! - `POST /agent-mgmt/agents/install`                       install_agent (multipart: file + metadata JSON)
//! - `POST /agent-mgmt/agents/install-from-url`              install_from_url (body JSON)
//! - `POST /agent-mgmt/agents/install-from-npm`              install_from_npm (body JSON)
//! - `POST /agent-mgmt/agents/uninstall`                     uninstall_agent (body JSON)
//!
//! # 参数传递约定
//!
//! 全部走 POST,body 解析:
//! - **简单 JSON 端点**:使用 [`I18nJsonOrQuery`] 提取器,优先 JSON body,兼容 `?project_id=xxx` query 调试
//! - **`install` 端点**:使用 `multipart/form-data`,字段:
//!   - `file`: 二进制文件(单文件 / tar.gz / zip)
//!   - `metadata`: JSON 字符串(含 `project_id` / `agent`(`agent_id` / `command` / `args` / `version`) / `install_type` / `source_url` / `npm_package` / `sha256`)
//!
//! # 错误模型
//!
//! 所有错误用 `AppError` 表达(axum 自动映射成 HTTP 状态 + 业务错误码 JSON)。
//! 18 个 agent-runner 业务码 + 2 个转发层专用码(见 `error_codes`)。

mod helpers;
mod install;
mod query;

pub use install::*;
pub use query::*;

// 共享转发 helper（validate_routing_params / resolve_container_target / build_ctx）
// 在 helpers.rs（query/install 两域共用）。

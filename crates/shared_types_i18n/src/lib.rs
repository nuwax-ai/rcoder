//! shared_types_i18n - 国际化支持模块
//!
//! 本 crate 提供多语言支持、错误码定义。

// 初始化 rust-i18n
rust_i18n::i18n!("locales", fallback = "en-US");

// i18n 模块
pub mod i18n;
pub use i18n::{
    get_locale, parse_accept_language, set_locale, t,
    t_default, DEFAULT_LOCALE, SUPPORTED_LOCALES,
};

// 错误码模块
pub mod error_codes;
pub use error_codes::{
    get_error_message, get_i18n_message, get_i18n_message_default, get_error_description,
    SUCCESS, ERR_AGENT_BUSY, ERR_CANCEL_FAILED, ERR_STOP_FAILED, ERR_VALIDATION,
    ERR_INVALID_PARAMS, ERR_INVALID_RESOURCE_LIMITS, ERR_CONTAINER_ERROR,
    ERR_WORKSPACE_ERROR, ERR_GRPC_ADDR_ERROR, ERR_GRPC_ERROR, ERR_SERVICE_UNAVAILABLE,
    ERR_AGENT_ERROR, ERR_PROXY_DISABLED, ERR_PROXY_SERVICE_UNAVAILABLE, ERR_UNKNOWN,
    ERR_SESSION_NOT_FOUND, ERR_AGENT_NOT_FOUND, ERR_CONTAINER_NOT_FOUND,
    ERR_HTTP_FALLBACK_FAILED, ERR_INTERNAL_SERVER_ERROR, ERR_RESUME_FAILED,
    ERR_RETRY_EXHAUSTED, ERR_TOO_MANY_REQUESTS, ERR_API_KEY_AUTH_FAILED,
    ERR_PERMISSION_NOT_FOUND, ERR_PERMISSION_RESOLVE_FAILED, ERR_PERMISSION_EXPIRED,
};

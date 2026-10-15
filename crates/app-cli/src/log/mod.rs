//! 日志写入留在 app-cli；纯查询、游标和来源解析由共享 reader 实现。
pub use userapp_log_reader::{filter, model, read, service, sources};

pub mod reader;
pub mod writer;

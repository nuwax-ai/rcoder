//! 日志系统：轮转写入 + 历史读取 + SSE 实时流。
//!
//! - [`writer`]：RotatingWriter（append + 大小轮转）+ pipe_to_rotating_file
//! - [`reader`]：list_log_files + read_last_n_lines + read_from_offset

pub mod reader;
pub mod model;
pub mod service;
pub mod writer;

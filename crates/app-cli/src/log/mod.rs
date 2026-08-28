//! 日志系统：轮转写入 + 历史读取 + SSE 实时流。
//!
//! - [`writer`]：RotatingWriter（append + 大小轮转）+ pipe_to_rotating_file
//! - [`reader`]：list_log_files + read_last_n_lines + read_from_offset
//! - [`service`]：LogService 查询编排（select/match_files/read_source/cursor）
//! - [`sources`]：日志源解析（runtime 源注入 / 目录布局 / 源类型）
//! - [`read`]：分页读取引擎（多行合并 / 行截断 / cursor 回退）
//! - [`filter`]：记录过滤与时间戳处理

pub mod filter;
pub mod model;
pub mod read;
pub mod reader;
pub mod service;
pub mod sources;
pub mod writer;

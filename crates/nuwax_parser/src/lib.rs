//! Nuwax Parser - 高性能 Rust 文件解析和同步工具包
//!
//! 专为 Nuwax 平台设计，支持前端和后端系统之间的无缝文件同步。
//! 内置支持哈希验证、URL文件下载和隐藏目录过滤功能。
//!
//! ## 主要功能
//!
//! - **多格式支持**: 支持 CSS、TypeScript React、JavaScript、JSON、图片（JPG/PNG）和纯文本文件
//! - **哈希验证**: 基于 SHA256 的文件完整性检查
//! - **URL文件下载**: 自动下载远程文件（图片、资源文件）
//! - **隐藏目录过滤**: 自动排除以 "." 开头的目录（如 .claude）
//! - **WASM兼容**: 可编译为 WebAssembly 供前端使用
//! - **文件同步**: 智能双向文件同步，支持变更检测

pub mod model;
pub mod project_op;
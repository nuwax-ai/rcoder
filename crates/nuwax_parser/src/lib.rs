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
//!
//! ## 快速开始
//!
//! ```rust
//! use nuwax_parser::project_to_v0_result;
//! use std::path::PathBuf;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let project_path = PathBuf::from("./my-project");
//!     let v0_result = project_to_v0_result(&project_path, true).await?;
//!     println!("Found {} files", v0_result.files.len());
//!     Ok(()) 
//! }
//! ```

// 重新导出所有公共接口
pub use types::*;
pub use utils::*;
pub use sync::*;

// 模块声明
mod types;
mod parsing;
mod sync;
mod utils;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_hash() {
        let content = "test content";
        let hash = calculate_hash(content);
        assert_eq!(hash.len(), 64); // SHA256 produces 64 character hex string
    }

    #[test]
    fn test_determine_file_type() {
        // 注意：这个测试需要访问私有函数，我们可以在 utils 模块中添加公共测试
        // 或者重新设计为测试公共接口
    }
}
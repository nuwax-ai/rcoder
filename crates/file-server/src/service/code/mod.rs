//! code 文件写操作 (对齐 nuwax `codeService.specifiedFilesUpdate` / `allFilesUpdate`)。
//!
//! 拆分: [`codec`] (URI 编解码) / [`specified`] (增量操作 + apply_file_ops +
//! ModifyStrategy + 行级 diff) / [`all_files`] (全量覆盖 + prune)。请求结构
//! (FileOp/FileEntry/FileOperation) 在 [`crate::models::code`]。

mod all_files;
mod codec;
mod specified;

pub use all_files::{AllResult, all_files_update, apply_all_files};
pub use codec::{decode_uri_component, encode_uri_component};
pub use specified::{ModifyStrategy, SpecifiedResult, apply_file_ops, specified_files_update};

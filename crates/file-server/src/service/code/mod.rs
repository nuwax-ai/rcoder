//! code 文件写操作 (对齐 nuwax `codeService.specifiedFilesUpdate` / `allFilesUpdate`)。
//!
//! 拆分: [`types`] (请求结构) / [`codec`] (URI 编解码) / [`specified`] (增量操作 +
//! apply_file_ops + ModifyStrategy + 行级 diff) / [`all_files`] (全量覆盖 + prune)。
//! 本 mod.rs 仅做模块声明 + 公共 API re-export, 保持外部 `code::*` 路径不变。

mod all_files;
mod codec;
mod specified;
mod types;

pub use all_files::{AllResult, all_files_update, apply_all_files};
pub use codec::{decode_uri_component, encode_uri_component};
pub use specified::{ModifyStrategy, SpecifiedResult, apply_file_ops, specified_files_update};
pub use types::{FileEntry, FileOp};

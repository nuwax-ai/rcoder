//! 文件树公开入口与共享路径边界工具。
//!
//! - `content`: project 内容读取及版本文件列表（保留既有空目录条目规则）
//! - `metadata`: computer 轻量文件列表（目录条目始终输出，按类型/深度/limit 遍历）
//! - `entries`: 共享目录读取、过滤与排序，明确区分列表与 project 的排除规则
//! - `framework`: 前端/构建框架检测
//! - `resolve`: resolve-file 路径解析
//! - `search`: 无索引有界实时搜索

mod content;
mod entries;
mod framework;
mod metadata;
pub mod resolve;
pub mod search;

pub use content::{get_project_content, list_files};
pub use metadata::{list_files_meta, list_files_meta_filtered};
pub use resolve::{FileResolveResult, resolve_existing_file};
pub use search::{SearchKindFilter, SearchParams, SearchResult, search_files};

use std::path::{Component, Path, PathBuf};

use path_clean::PathClean;
use serde::Serialize;

use crate::error::{AppError, AppResult};
use crate::path_safety;

/// 遍历时保留的唯一隐藏文件 (其余 `.` 开头的文件跳过)。
pub(crate) const KEEP_HIDDEN_FILE: &str = ".gitignore";

// FileEntry 移至 models（wire 契约 + ToSchema）；此处 re-export 保持
// `tree::FileEntry` 既有引用路径。
pub use crate::models::FileTreeEntry as FileEntry;

#[derive(Serialize)]
pub struct ProjectContent {
    pub files: Vec<FileEntry>,
    pub frontend_framework: String,
    pub dev_framework: String,
}

/// 文件列表输出类型；`type=file` 时目录条目不输出但遍历仍下钻。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MetaListType {
    #[default]
    All,
    File,
    Dir,
}

impl MetaListType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::File => "file",
            Self::Dir => "dir",
        }
    }

    /// 解析归一化后的类型串 (小写、已 trim): `""`/`all` → All、`file` → File、
    /// `dir`/`directory` → Dir；其余 `None`。错误契约 (文案/details.value) 由调用层
    /// 构造，纯转换在此独立测试。
    pub fn parse_normalized(normalized: &str) -> Option<Self> {
        match normalized {
            "" | "all" => Some(Self::All),
            "file" => Some(Self::File),
            "dir" | "directory" => Some(Self::Dir),
            _ => None,
        }
    }
}

impl std::fmt::Display for MetaListType {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MetaListOptions {
    pub recursive: bool,
    /// 受限展开层级 (TS 1.5.4 `depth`): 默认 1=纯单层; N>1 时单层模式下目录
    /// 向下再展开 N-1 级 (DFS, 目录条目后紧跟其子项)。递归模式忽略此值。
    pub levels_left: usize,
    pub file_type: MetaListType,
    pub limit: Option<usize>,
}

impl MetaListOptions {
    fn reached_limit(self, count: usize) -> bool {
        self.limit.is_some_and(|limit| count >= limit)
    }
}

/// 剥前导根/盘符前缀组件，转为相对形态（意图对齐 TS `replace(/^[\/\\]+/,"")`，
/// 实现用 [`Path::components()`] 而非手写字符集匹配）：POSIX `/`、`//`，
/// win32 `\`、`C:/` 等前导形态由 std 组件层统一归一。
///
/// 宿主语义差异（有意）：POSIX 上 `\` 是普通文件名字符（不是分隔符），
/// 组件层不剥——比 TS 的字符串剥更忠实于宿主文件系统；Windows 上 `\`
/// 是分隔符，照剥。返回相对形态字符串（可能为空，调用方自行判空）。
pub(crate) fn strip_leading_root_components(p: &str) -> String {
    Path::new(p)
        .components()
        .skip_while(|c| matches!(c, Component::RootDir | Component::Prefix(_)))
        .collect::<PathBuf>()
        .to_string_lossy()
        .into_owned()
}

/// 解析 `relative_path` 到 `root` 内的子目录绝对路径 (对齐 TS `resolvePathWithinWorkspace`)。
///
/// 用 [`path_clean::PathClean`] 标准化路径 (等价 TS `path.normalize`), 消除 `.`/`..`:
/// - `None` / 纯空白 / 仅分隔符 (`"/"`) / `"."` → `root` 本身;
/// - 前导根组件剥离 → 兼容 `"/sub"` 这类写法 (见 [`strip_leading_root_components`]);
/// - 标准化后仍含 `..` (即 `..` 未被抵消, 越出根) → `Err`;
/// - `ensure_within_path` (clean + starts_with) 做最终兜底, 双重保险。
///
/// 有意偏离 TS：不做整体 trim——会静默变形以空白开头/结尾的合法路径段；
/// 判空保持 TS 口径（纯空白视同未指定），非纯空白原样解析。
pub(crate) fn resolve_subdir(root: &Path, relative_path: Option<&str>) -> AppResult<PathBuf> {
    let Some(rel) = relative_path.filter(|s| !s.trim().is_empty()) else {
        return Ok(root.clean());
    };

    let stripped = strip_leading_root_components(rel);
    if stripped.is_empty() {
        return Ok(root.clean());
    }

    // 标准化: 消除 . 和能抵消的 .. (如 "a/../b" → "b")。未抵消的 .. 会保留。
    let normalized = Path::new(&stripped).clean();

    // 标准化后仍含 .. → 越界 (如 "../x" 不会被抵消)。用 components 检测最可靠。
    if normalized
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(illegal_relative_path(relative_path));
    }

    // normalized 是相对路径 (已剥前导斜杠 + 无 ..), join 不会替换 base;
    // ensure_within_path 做最终 starts_with 兜底, 双重保险。
    path_safety::ensure_within_path(&root.clean(), normalized)
}

/// `relativePath` 越界的 details 结构 (JSON 键对齐 TS 契约): 字段集编译期锁定。
#[derive(serde::Serialize)]
struct RelativePathErrorDetails<'a> {
    field: &'static str,
    #[serde(rename = "relativePath")]
    relative_path: &'a str,
}

/// `relativePath` 越界的 ValidationError: 字面 `..` 穿越 (resolve_subdir) 与
/// realpath 级越界目录链接 (ensure_resolved_list_dir, TS e822516) 共用同一
/// 报错口径 (message 英文; details.field/relativePath 对齐 TS 键名)。
pub(crate) fn illegal_relative_path(relative_path: Option<&str>) -> AppError {
    AppError::validation_with_details(
        "relativePath is not safe, cannot exceed target directory",
        RelativePathErrorDetails {
            field: "relativePath",
            relative_path: relative_path.unwrap_or(""),
        },
    )
}

/// realpath 级列表/搜索起点边界 (对齐 TS e822516 escapesRoot): `resolved` 是
/// `resolve_subdir` 字面解析后的绝对路径, 经符号链接 (含目录链接中段) 解析后
/// 越出 `root` → 与字面 `..` 穿越同款 400。缺失路径不在此层报错。
pub(crate) async fn ensure_resolved_list_dir(
    root: &Path,
    resolved: &Path,
    relative_path: Option<&str>,
) -> AppResult<()> {
    path_safety::ensure_resolved_target_within(root, resolved)
        .await
        .map_err(|_| illegal_relative_path(relative_path))
}

/// 构造 fileProxyUrl 的 path 段 (对齐 TS `buildFileProxyUrl` 的 path 部分):
/// `${proxyPath}/${逐段 enc}`。customTargetDir 后缀由 handler 统一追加。
/// `proxy_path`/`relative` 为空 → `None`。
pub(super) fn build_file_proxy_url(proxy_path: Option<&str>, relative: &str) -> Option<String> {
    let p = proxy_path?;
    if relative.is_empty() {
        return None;
    }
    Some(format!("{p}/{}", encode_path_segments(relative)))
}

/// 相对路径逐段 encodeURIComponent (对齐 nuwax traverseDirectory 的 fileProxyUrl 构造:
/// `relativePath.split("/").map(encodeURIComponent).join("/")`)。
fn encode_path_segments(rel: &str) -> String {
    rel.split('/')
        .map(crate::service::code::encode_uri_component)
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests;

//! 共享目录读取、过滤与排序；调用方显式选择目录排除策略。

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::config::Config;
use crate::error::AppResult;

use super::KEEP_HIDDEN_FILE;

/// 排除目录名命中 (对齐 TS 0a7417f `isTraverseExcludedDir`): Windows 文件名不区分
/// 大小写 (Node_Modules 与 node_modules 是同一目录), 忽略大小写匹配; POSIX 区分
/// 大小写, 精确匹配 (Linux 下 Dist 是合法的另一个目录, 不能误杀)。
pub(super) fn is_traverse_excluded_dir(exclude_dirs: &[String], name: &str) -> bool {
    if cfg!(windows) {
        let lowered = name.to_lowercase();
        exclude_dirs.iter().any(|d| d.to_lowercase() == lowered)
    } else {
        exclude_dirs.iter().any(|d| d == name)
    }
}

/// computer 元信息列表使用宿主平台规则；project 内容链路保留 TS 的精确匹配。
#[derive(Clone, Copy)]
pub(super) enum DirectoryExcludePolicy {
    PlatformAware,
    Exact,
}

impl DirectoryExcludePolicy {
    fn excludes(self, exclude_dirs: &[String], name: &str) -> bool {
        match self {
            Self::PlatformAware => is_traverse_excluded_dir(exclude_dirs, name),
            Self::Exact => exclude_dirs.iter().any(|excluded| excluded == name),
        }
    }
}

/// 读取目录条目并按 nuwax 规则过滤 + 排序 (隐藏文件除 .gitignore / traverse_exclude_dirs /
/// content_traverse_exclude_files; 目录在前 + 名字大小写不敏感)。供递归/单层遍历复用。
/// 保留 symlink 作为叶条目供 metadata 列表报告 `isLink`；读取文件内容的遍历会单独跳过它们。
pub(super) async fn read_filtered_entries(
    dir: &Path,
    config: &Config,
    directory_exclude_policy: DirectoryExcludePolicy,
) -> AppResult<Vec<(String, PathBuf, bool, bool)>> {
    let mut entries = fs::read_dir(dir).await?;
    let mut items: Vec<(String, PathBuf, bool, bool)> = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        // 隐藏文件 (除 .gitignore) 跳过 (对齐 nuwax computer traverseDirectory)
        if name.starts_with('.') && name != KEEP_HIDDEN_FILE {
            continue;
        }
        let ft = entry.file_type().await?;
        let path = entry.path();
        let is_link = ft.is_symlink();
        if ft.is_dir() && !directory_exclude_policy.excludes(&config.traverse_exclude_dirs, &name) {
            items.push((name, path, true, is_link));
        } else if (ft.is_file() || is_link)
            && !config
                .content_traverse_exclude_files
                .iter()
                .any(|f| f == &name)
        {
            items.push((name, path, false, is_link));
        }
    }
    // 排序: 目录在前, 名字大小写不敏感 (对齐 nuwax localeCompare)
    items.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    Ok(items)
}

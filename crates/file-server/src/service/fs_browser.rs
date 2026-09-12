//! 文件系统目录浏览（目录选择弹窗用，对齐 TS f979df7 `fsBrowserUtils.js`）。
//!
//! **信任模型**：按绝对路径列一层子项，**不锚定工作空间、不带会话上下文**——
//! 与 `resolveServiceContext` 体系无关（TS 侧同款设计：两个 /fs 端点不解析
//! service，multer 临时目录也不走该体系）。绝对路径校验用宿主语义
//! `Path::is_absolute`（等价 TS `path.isAbsolute`，两侧恰好同构；区别于
//! `workspace::normalize_workspace_path` 的平台无关字符串规则）。
//!
//! 跨平台：mac/Linux 根为 `/`，Windows 根为盘符列表；返回路径分隔符统一为 `/`
//! （与 workspacePath 校验/落库的规范化一致，Windows 侧 fs 均接受正斜杠）。

use crate::error::{AppError, AppResult};
use crate::models::response::{FsEntry, FsRootEntry};

/// 分隔符统一为 `/` 并折叠连续分隔符（保留 UNC 的前导 `//`）——镜像 TS
/// `toDisplayPath`；与 `workspace::canonicalize_dir` 的差异：**不做**盘符大写
/// （TS 侧也是两份独立实现，display 展示保留原大小写）。
pub(crate) fn to_display_path(p: &str) -> String {
    let unc = p.starts_with("//") || p.starts_with("\\\\");
    let mut collapsed = String::with_capacity(p.len());
    let mut in_sep = false;
    for ch in p.chars() {
        let normalized = if ch == '\\' { '/' } else { ch };
        if normalized == '/' {
            if !in_sep {
                collapsed.push('/');
            }
            in_sep = true;
        } else {
            collapsed.push(normalized);
            in_sep = false;
        }
    }
    if unc {
        collapsed.insert(0, '/');
    }
    collapsed
}

/// 自然排序比较器：数字段按数值（去前导零后先长度后字典）、非数字段小写比较——
/// TS `localeCompare(undefined, {sensitivity:"base", numeric:true})` 的近似实现
/// （ASCII 域内等价；CJK 整序按码点，与 ICU collation 有差异，注释锚定）。
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let ac = split_chunks(a);
    let bc = split_chunks(b);
    for ((a_digit, a_chunk), (b_digit, b_chunk)) in ac.iter().zip(bc.iter()) {
        let ord = if *a_digit && *b_digit {
            let an = a_chunk.trim_start_matches('0');
            let bn = b_chunk.trim_start_matches('0');
            an.len().cmp(&bn.len()).then_with(|| an.cmp(bn))
        } else {
            let al: String = a_chunk.chars().flat_map(char::to_lowercase).collect();
            let bl: String = b_chunk.chars().flat_map(char::to_lowercase).collect();
            al.cmp(&bl)
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    ac.len().cmp(&bc.len())
}

/// 切分为「数字段 / 非数字段」交替的块（自然排序的基本单位）。
fn split_chunks(s: &str) -> Vec<(bool, String)> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_digit: Option<bool> = None;
    for ch in s.chars() {
        let is_digit = ch.is_ascii_digit();
        match current_digit {
            Some(d) if d == is_digit => current.push(ch),
            _ => {
                if !current.is_empty() {
                    chunks.push((current_digit.unwrap_or(false), std::mem::take(&mut current)));
                }
                current_digit = Some(is_digit);
                current.push(ch);
            }
        }
    }
    if !current.is_empty() {
        chunks.push((current_digit.unwrap_or(false), current));
    }
    chunks
}

/// Windows 盘符候选 `A:/` ~ `Z:/`（纯函数，跨平台可测；探测在
/// [`probe_drive_roots`]，仅此薄层 cfg(windows)）。
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "仅 Windows 盘符探测使用; 单测跨平台验证其生成")
)]
pub(crate) fn drive_candidates() -> Vec<String> {
    (b'A'..=b'Z')
        .map(|code| format!("{}:/", code as char))
        .collect()
}

/// 浏览起点：根目录列表 + 用户 home。
///
/// - win32：逐一探测盘符存在性（对齐 TS `fs.promises.stat`）
/// - 其余平台：`/`
///
/// home 单独返回（前端快捷入口），`dirs::home_dir` 失败 → None（罕见）。
pub(crate) async fn list_fs_roots() -> AppResult<(Vec<FsRootEntry>, Option<String>)> {
    let roots = if cfg!(windows) {
        probe_drive_roots().await
    } else {
        vec![FsRootEntry {
            name: "/".to_string(),
            path: "/".to_string(),
            is_dir: true,
        }]
    };
    let home = dirs::home_dir().map(|h| to_display_path(&h.to_string_lossy()));
    Ok((roots, home))
}

/// Windows 盘符探测（不存在的盘符跳过）。函数本体全平台定义（调用点
/// `cfg!(windows)` 运行时分支），仅探测循环 cfg(windows)——非 Windows 编译
/// 含空函数，路径不可达。
async fn probe_drive_roots() -> Vec<FsRootEntry> {
    #[cfg(windows)]
    {
        let mut roots = Vec::new();
        for drive in drive_candidates() {
            if tokio::fs::metadata(&drive).await.is_ok() {
                roots.push(FsRootEntry {
                    name: drive.clone(),
                    path: drive,
                    is_dir: true,
                });
            }
        }
        roots
    }
    #[cfg(not(windows))]
    Vec::new()
}

/// 列出目录下一层子项（目录 + 文件），目录在前、名称自然排序（大小写不敏感）。
///
/// - 隐藏文件（`.` 开头）默认返回（前端决定展示样式）
/// - 符号链接按目标类型展示（指向目录则可进入），并标记 `is_symlink`
/// - 目录不存在 / 无权限 / 不是目录 → 400（`ValidationError` 同款信封）
pub(crate) async fn list_fs_children(dir_path: &str) -> AppResult<(String, Vec<FsEntry>)> {
    if dir_path.is_empty() || dir_path.contains('\0') {
        return Err(AppError::validation("path is required"));
    }
    // 宿主语义绝对路径（file-server 与目标机器同机运行, 对齐 TS 本机校验）
    if !std::path::Path::new(dir_path).is_absolute() {
        return Err(AppError::validation("path must be absolute"));
    }
    let mut dir = tokio::fs::read_dir(dir_path)
        .await
        .map_err(|error| AppError::validation(format!("cannot read directory ({error})")))?;
    let mut entries = Vec::new();
    while let Some(entry) = dir
        .next_entry()
        .await
        .map_err(|error| AppError::validation(format!("cannot read directory ({error})")))?
    {
        // file_type 来自 dirent (不跟随链接); 失败按文件展示 (防御, 对齐 TS 容错)
        let file_type = entry.file_type().await.ok();
        let is_symlink = file_type.as_ref().is_some_and(|t| t.is_symlink());
        let mut is_dir = file_type.as_ref().is_some_and(|t| t.is_dir());
        if is_symlink {
            // 链接 → 追 metadata 定目标类型; 悬空链接按文件 (前端置灰)
            is_dir = tokio::fs::metadata(entry.path())
                .await
                .map(|m| m.is_dir())
                .unwrap_or(false);
        }
        entries.push(FsEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: to_display_path(&entry.path().to_string_lossy()),
            is_dir,
            is_symlink,
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| natural_cmp(&a.name, &b.name))
    });
    Ok((to_display_path(dir_path), entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_path_collapses_separators_and_keeps_unc() {
        assert_eq!(to_display_path("/a//b"), "/a/b");
        assert_eq!(to_display_path("a\\b//c"), "a/b/c");
        assert_eq!(to_display_path("\\\\srv\\share\\x"), "//srv/share/x");
        assert_eq!(to_display_path("//srv/share/x"), "//srv/share/x");
        // display 层不做盘符大写 (与 canonicalize_dir 的差异)
        assert_eq!(to_display_path("c:/x"), "c:/x");
    }

    #[test]
    fn natural_cmp_orders_numbers_and_case() {
        use std::cmp::Ordering::*;
        assert_eq!(natural_cmp("a2", "a10"), Less); // 数值序 (非字典序)
        assert_eq!(natural_cmp("B", "a"), Greater); // 折叠后 b > a
        assert_eq!(natural_cmp("B", "b"), Equal); // 大小写不敏感 (B == b)
        assert_eq!(natural_cmp("img9.png", "img10.png"), Less);
        assert_eq!(natural_cmp("a", "a1"), Less); // 前缀规则
        assert_eq!(natural_cmp("a01", "a1"), Equal); // 前导零等价
    }

    #[test]
    fn drive_candidates_cover_az() {
        let drives = drive_candidates();
        assert_eq!(drives.len(), 26);
        assert_eq!(drives.first().map(String::as_str), Some("A:/"));
        assert_eq!(drives.last().map(String::as_str), Some("Z:/"));
    }

    #[tokio::test]
    async fn roots_on_posix_are_slash_and_home() {
        // 非 Windows 平台: roots=[/], home 以 / 开头 (Windows 分支交 CI 验证)
        if cfg!(windows) {
            return;
        }
        let (roots, home) = list_fs_roots().await.expect("roots");
        assert_eq!(roots.len(), 1);
        assert_eq!((roots[0].name.as_str(), roots[0].path.as_str()), ("/", "/"));
        assert!(home.expect("home on test hosts").starts_with('/'));
    }

    #[tokio::test]
    async fn children_lists_dirs_first_with_symlink_semantics() {
        let tmp = tempfile::tempdir().expect("tempdir");
        tokio::fs::create_dir_all(tmp.path().join("zdir"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(tmp.path().join("adir"))
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("b10.txt"), "x")
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("b9.txt"), "x")
            .await
            .unwrap();
        // symlink → 目录 (可进入) 与悬空链接 (按文件)
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(tmp.path().join("adir"), tmp.path().join("lnk-dir"))
                .unwrap();
            std::os::unix::fs::symlink("/nonexistent-dangling", tmp.path().join("lnk-dead"))
                .unwrap();
        }

        let (display_path, entries) = list_fs_children(tmp.path().to_str().expect("utf8 tmp"))
            .await
            .expect("children");

        assert_eq!(display_path, tmp.path().to_str().expect("utf8 tmp"));
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        // 目录在前 + 自然排序: adir, zdir (目录) → b9, b10 (数值序) → links
        #[cfg(unix)]
        assert_eq!(
            names,
            vec!["adir", "lnk-dir", "zdir", "b9.txt", "b10.txt", "lnk-dead"]
        );
        #[cfg(not(unix))]
        assert_eq!(names, vec!["adir", "zdir", "b9.txt", "b10.txt"]);

        #[cfg(unix)]
        {
            let lnk_dir = entries.iter().find(|e| e.name == "lnk-dir").unwrap();
            assert!(
                lnk_dir.is_dir && lnk_dir.is_symlink,
                "链接指向目录 → 可进入 + 标记"
            );
            let lnk_dead = entries.iter().find(|e| e.name == "lnk-dead").unwrap();
            assert!(
                !lnk_dead.is_dir && lnk_dead.is_symlink,
                "悬空链接 → 按文件展示"
            );
        }
        // 隐藏文件默认返回
        tokio::fs::write(tmp.path().join(".hidden"), "x")
            .await
            .unwrap();
        let (_, entries2) = list_fs_children(tmp.path().to_str().expect("utf8 tmp"))
            .await
            .expect("children again");
        assert!(entries2.iter().any(|e| e.name == ".hidden"));
    }

    #[tokio::test]
    async fn children_rejects_bad_paths_and_missing_dir() {
        for bad in ["", "relative/path", "/nonexistent-definitely-not-here-xyz"] {
            let err = list_fs_children(bad).await.expect_err(bad);
            assert!(!err.to_string().is_empty());
        }
        // \0 注入
        assert!(list_fs_children("/a\0b").await.is_err());
    }
}

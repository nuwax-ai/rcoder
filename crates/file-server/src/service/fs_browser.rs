//! 文件系统目录浏览与目录弹窗写操作（目录选择弹窗用，对齐 TS 1.5.3 `fsBrowserUtils.js`）。
//!
//! **信任模型**：按绝对路径操作，**不锚定工作空间、不带会话上下文**——
//! 与 `resolveServiceContext` 体系无关（TS 侧同款设计：/fs 端点不解析
//! service，multer 临时目录也不走该体系）。绝对路径校验用宿主语义
//! `Path::is_absolute`（等价 TS `path.isAbsolute`，两侧恰好同构；区别于
//! `workspace::normalize_workspace_path` 的平台无关字符串规则）。
//!
//! 跨平台：mac/Linux 根为 `/`，Windows 根为盘符列表；返回路径分隔符统一为 `/`
//! （与 workspacePath 校验/落库的规范化一致，Windows 侧 fs 均接受正斜杠）。

use crate::error::{AppError, AppResult};
use crate::models::response::{FsEntry, FsRootEntry};

/// mkdir/rename 的结果载荷（不含 `success` 信封，handler 组装 wire 响应；
/// `isDir`/`isSymlink` 为端点语义常量，由 handler 按端点填入）。
#[derive(Debug)]
pub(crate) struct FsMutatedDir {
    /// 原样保留的目录名
    pub name: String,
    /// 归一化后的完整路径
    pub path: String,
    /// 归一化后的父目录路径
    pub parent_path: String,
}

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

/// 绝对路径入参校验（浏览/新建/重命名共用）：非空、无 NUL、宿主语义绝对路径。
fn validate_absolute_path(dir_path: &str, field: &str) -> AppResult<()> {
    if dir_path.is_empty() || dir_path.contains('\0') {
        return Err(validation_field(format!("{field} is required"), field));
    }
    // 宿主语义绝对路径（file-server 与目标机器同机运行, 对齐 TS 本机校验）
    if !std::path::Path::new(dir_path).is_absolute() {
        return Err(validation_field(format!("{field} must be absolute"), field));
    }
    Ok(())
}

/// 校验目录/文件名（mkdir 的 dirName、rename 的 newName 共用），原样返回名字。
/// 仅用 trim 判断纯空白名，不改变有意义的首尾空格。
///
/// `/` 与 `\` 均拒绝：`\` 在 win32 是分隔符，且 `to_display_path` 会把 `\`
/// 归一为 `/`，POSIX 下合法的反斜杠名会导致回显路径与实际路径错乱。
///
/// 长度上限按宿主文件系统真实约束（比 TS 的 UTF-16 length 更准）：POSIX
/// 文件系统（ext4/APFS/…）分量上限 255 **字节**（UTF-8），NTFS 为 255 个
/// UTF-16 码元——分别判定，不误伤宿主上合法的名字，超限名提前拿到干净的
/// "exceeds 255 characters" 而非 OS 报错兜底。
fn validate_entry_name(name: &str, field: &str) -> AppResult<String> {
    if name.trim().is_empty() || name.contains('\0') {
        return Err(validation_field(format!("{field} is required"), field));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(validation_field(
            format!("{field} must not contain path separators"),
            field,
        ));
    }
    if name == "." || name == ".." {
        return Err(validation_field(
            format!("{field} must not be a relative segment"),
            field,
        ));
    }
    #[cfg(windows)]
    let too_long = name.encode_utf16().count() > 255;
    #[cfg(not(windows))]
    let too_long = name.len() > 255;
    if too_long {
        return Err(validation_field(
            format!("{field} exceeds 255 characters"),
            field,
        ));
    }
    Ok(name.to_string())
}

/// ValidationError 且带 `field` 定位（对齐 TS `ValidationError(msg, {field})`
/// 信封，前端按 `details.field` 高亮输入框）。
fn validation_field(msg: impl Into<String>, field: &str) -> AppError {
    AppError::validation_with(msg, serde_json::json!({ "field": field }))
}

/// 列出目录下一层子项（目录 + 文件），目录在前、名称自然排序（大小写不敏感）。
///
/// - 隐藏文件（`.` 开头）默认返回（前端决定展示样式）
/// - 符号链接按目标类型展示（指向目录则可进入），并标记 `is_symlink`
/// - 目录不存在 / 无权限 / 不是目录 → 400（`ValidationError` 同款信封）
pub(crate) async fn list_fs_children(dir_path: &str) -> AppResult<(String, Vec<FsEntry>)> {
    validate_absolute_path(dir_path, "path")?;
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

/// 在 parent_path 下新建一层目录（目录选择弹窗"新建文件夹"）。
///
/// 非递归：父目录须已存在（来自浏览选择）；名称支持中文等任意合法文件名。
/// 重名 / 父目录不存在等以 ValidationError 透出给前端提示。
pub(crate) async fn create_fs_directory(
    parent_path: &str,
    dir_name: &str,
) -> AppResult<FsMutatedDir> {
    validate_absolute_path(parent_path, "parentPath")?;
    let name = validate_entry_name(dir_name, "dirName")?;
    let target = std::path::Path::new(parent_path).join(&name);
    // 非递归 create_dir：父目录不存在报 NotFound，对齐 TS fs.mkdir（不带 recursive）
    match tokio::fs::create_dir(&target).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(validation_field(
                format!("directory already exists: {name}"),
                "dirName",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(validation_field(
                "parent directory does not exist",
                "parentPath",
            ));
        }
        Err(error) => {
            return Err(validation_field(
                format!("cannot create directory ({error})"),
                "dirName",
            ));
        }
    }
    Ok(FsMutatedDir {
        path: to_display_path(&target.to_string_lossy()),
        name,
        parent_path: to_display_path(parent_path),
    })
}

/// 同目录重命名（目录选择弹窗）。new_name 仅是名字（校验拒绝分隔符），不支持跨目录移动。
///
/// 目标名先预检再 rename：POSIX 的 rename 指向已存在空目录时会静默替换，预检
/// 保证跨平台一致的 "already exists" 报错。预检用 `symlink_metadata`（lstat 语义，
/// 有意比 TS 的 stat 更保守）：悬空符号链接也判为已存在，避免 rename 静默替换
/// 链接本体。预检与 rename 之间存在 TOCTOU 窗口（TS 同款取舍；目录弹窗单用户
/// 场景可接受，最坏情况由 OS rename 报错兜底）。大小写不敏感文件系统（macOS/
/// Windows）上仅改大小写的重命名会被判为重名而拒绝，属可接受的取舍。
pub(crate) async fn rename_fs_directory(dir_path: &str, new_name: &str) -> AppResult<FsMutatedDir> {
    validate_absolute_path(dir_path, "path")?;
    let name = validate_entry_name(new_name, "newName")?;
    let source = std::path::Path::new(dir_path);
    // parent() 对 POSIX `/` 与 win32 `C:/` 等根目录返回 None（对齐 TS
    // `dirname(p) === p` 的根目录拒绝，且无需字符串比较）
    let Some(parent) = source.parent() else {
        return Err(validation_field("cannot rename the root directory", "path"));
    };
    let target = parent.join(&name);
    match tokio::fs::symlink_metadata(&target).await {
        Ok(_) => {
            return Err(validation_field(
                format!("name already exists: {name}"),
                "newName",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(validation_field(format!("cannot rename ({error})"), "path"));
        }
    }
    match tokio::fs::rename(source, &target).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(validation_field("directory does not exist", "path"));
        }
        Err(error) => {
            return Err(validation_field(
                format!("cannot rename directory ({error})"),
                "newName",
            ));
        }
    }
    Ok(FsMutatedDir {
        path: to_display_path(&target.to_string_lossy()),
        name,
        parent_path: to_display_path(&parent.to_string_lossy()),
    })
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

        // 期望值经 to_display_path 归一（Windows 反斜杠 → `/`），与返回值同口径
        assert_eq!(
            display_path,
            to_display_path(tmp.path().to_str().expect("utf8 tmp"))
        );
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

    #[cfg(not(windows))]
    #[tokio::test]
    async fn children_serves_dir_with_trailing_space_verbatim() {
        // 尾空格目录名合法：路径原样使用（不做 trim），浏览带尾空格的目录本身
        // 成功，且从父目录能列出该条目
        let tmp = tempfile::tempdir().expect("tempdir");
        tokio::fs::create_dir_all(tmp.path().join("dir "))
            .await
            .unwrap();
        let raw = tmp.path().join("dir ").to_string_lossy().into_owned();
        let (display, _) = list_fs_children(&raw).await.expect("children of dir-space");
        assert_eq!(display, to_display_path(&raw));
        let (_, parent_entries) = list_fs_children(tmp.path().to_str().expect("utf8 tmp"))
            .await
            .expect("children");
        assert!(parent_entries.iter().any(|e| e.name == "dir "));
    }

    #[test]
    fn entry_name_validation_rejects_bad_input() {
        // 合法名：首尾空白是名字的一部分；中文名等任意合法文件名
        assert_eq!(
            validate_entry_name("  新建目录 ", "dirName").expect("valid"),
            "  新建目录 "
        );
        for bad in ["", "   ", "a/b", "a\\b", ".", "..", "a\0b"] {
            let err = validate_entry_name(bad, "dirName").expect_err(bad);
            assert!(!err.to_string().is_empty(), "bad={bad:?}");
        }
        let long = "x".repeat(256);
        assert!(validate_entry_name(&long, "dirName").is_err());
        assert!(validate_entry_name(&"x".repeat(255), "dirName").is_ok());
        // 长度上限按宿主约束：POSIX 255 UTF-8 字节 / Windows 255 UTF-16 码元
        // （255 字节 = 85 个三字节 CJK 字符，恰好不超；86 个在 POSIX 超字节限）
        let cjk_boundary: String = "文".repeat(85);
        assert!(validate_entry_name(&cjk_boundary, "dirName").is_ok());
        let cjk_over_bytes = "文".repeat(86);
        #[cfg(not(windows))]
        assert!(validate_entry_name(&cjk_over_bytes, "dirName").is_err());
        #[cfg(windows)]
        assert!(validate_entry_name(&cjk_over_bytes, "dirName").is_ok());
    }

    #[tokio::test]
    async fn mkdir_creates_one_level_and_reports_conflicts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent = tmp.path().to_string_lossy().into_owned();
        // 期望值经 to_display_path 归一（Windows 反斜杠 → `/`），与返回值同口径
        let display_parent = to_display_path(&parent);

        // 成功：中文名；返回 display 路径与父路径
        let dir = create_fs_directory(&parent, "新建目录")
            .await
            .expect("mkdir");
        assert_eq!(dir.name, "新建目录");
        assert_eq!(dir.parent_path, display_parent);
        assert!(dir.path.starts_with(&display_parent));
        assert!(dir.path.ends_with("新建目录"));
        assert!(
            tokio::fs::try_exists(tmp.path().join("新建目录"))
                .await
                .unwrap()
        );

        // 重名 → already exists
        let err = create_fs_directory(&parent, "新建目录")
            .await
            .expect_err("duplicate");
        assert!(err.to_string().contains("already exists"));

        // 目标为同名文件：create_dir 同样报 AlreadyExists（文件也占名）
        tokio::fs::write(tmp.path().join("occupied-file"), "x")
            .await
            .unwrap();
        let err = create_fs_directory(&parent, "occupied-file")
            .await
            .expect_err("file occupies name");
        assert!(err.to_string().contains("already exists"));

        // 非递归：父目录不存在 → parent directory does not exist
        let err = create_fs_directory(&format!("{parent}/no-such-dir"), "child")
            .await
            .expect_err("missing parent");
        assert!(err.to_string().contains("parent directory does not exist"));

        // 非法名与相对父路径
        let err = create_fs_directory(&parent, "a/b")
            .await
            .expect_err("separator");
        assert!(err.to_string().contains("path separators"));
        let err = create_fs_directory("relative", "x")
            .await
            .expect_err("relative parent");
        assert!(err.to_string().contains("must be absolute"));
    }

    #[tokio::test]
    async fn rename_within_same_directory_and_reports_conflicts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent = tmp.path();
        tokio::fs::create_dir(parent.join("old")).await.unwrap();
        tokio::fs::create_dir(parent.join("occupied"))
            .await
            .unwrap();

        // 成功：同目录改名，返回新路径与父路径（父路径为 display 归一口径）
        let dir = rename_fs_directory(&parent.join("old").to_string_lossy(), "新名字")
            .await
            .expect("rename");
        assert_eq!(dir.name, "新名字");
        assert_eq!(dir.parent_path, to_display_path(&parent.to_string_lossy()));
        assert!(tokio::fs::try_exists(parent.join("新名字")).await.unwrap());
        assert!(!tokio::fs::try_exists(parent.join("old")).await.unwrap());

        // 目标已存在 → name already exists
        let err = rename_fs_directory(&parent.join("新名字").to_string_lossy(), "occupied")
            .await
            .expect_err("conflict");
        assert!(err.to_string().contains("already exists"));

        // 源不存在 → directory does not exist
        let err = rename_fs_directory(&parent.join("never-existed").to_string_lossy(), "whatever")
            .await
            .expect_err("missing source");
        assert!(err.to_string().contains("does not exist"));

        // 根目录拒绝（POSIX `/` 与 win32 盘符根的 parent() 均为 None）
        let err = rename_fs_directory("/", "x").await.expect_err("root");
        assert!(err.to_string().contains("root directory"));

        // newName 含分隔符 → 拒绝（不支持跨目录移动）
        let err = rename_fs_directory(&parent.join("新名字").to_string_lossy(), "../escape")
            .await
            .expect_err("separator");
        assert!(err.to_string().contains("path separators"));

        // 改成自己的名字：目标即源本身，预检判为已存在（TS 同款行为）
        let err = rename_fs_directory(&parent.join("新名字").to_string_lossy(), "新名字")
            .await
            .expect_err("rename to self");
        assert!(err.to_string().contains("already exists"));
        assert!(tokio::fs::try_exists(parent.join("新名字")).await.unwrap());

        // 悬空符号链接目标：预检判为已存在，不静默替换链接（unix）
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                "/nonexistent-dangling-target",
                parent.join("dangling-link"),
            )
            .unwrap();
            let err =
                rename_fs_directory(&parent.join("新名字").to_string_lossy(), "dangling-link")
                    .await
                    .expect_err("dangling link target");
            assert!(err.to_string().contains("already exists"));
            // 源目录未被移动
            assert!(tokio::fs::try_exists(parent.join("新名字")).await.unwrap());
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn mkdir_and_rename_preserve_name_whitespace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent = tmp.path().to_string_lossy();
        let created = create_fs_directory(&parent, "  新建目录 ")
            .await
            .expect("create spaced name");
        assert_eq!(created.name, "  新建目录 ");
        assert!(tmp.path().join("  新建目录 ").is_dir());

        let renamed = rename_fs_directory(&created.path, " 新名字 ")
            .await
            .expect("rename spaced name");
        assert_eq!(renamed.name, " 新名字 ");
        assert!(tmp.path().join(" 新名字 ").is_dir());
        assert!(!tmp.path().join("  新建目录 ").exists());
    }
}

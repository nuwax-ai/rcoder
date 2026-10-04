use std::path::{Path, PathBuf};

use tokio::fs;

use crate::config::Config;
use crate::error::AppError;

use super::entries::is_traverse_excluded_dir;
use super::*;

fn default_test_config() -> Config {
    Config::default()
}

/// 构造测试目录结构 (供 list_files_meta / resolve / search 测试共享):
/// ```text
/// root/
///   a.txt
///   b.md
///   sub/
///     c.txt
///     d.log
///     nested/
///       e.txt
/// ```
async fn make_test_tree(root: &Path) {
    fs::create_dir_all(root.join("sub").join("nested"))
        .await
        .unwrap();
    fs::write(root.join("a.txt"), "a").await.unwrap();
    fs::write(root.join("b.md"), "b").await.unwrap();
    fs::write(root.join("sub").join("c.txt"), "c")
        .await
        .unwrap();
    fs::write(root.join("sub").join("d.log"), "d")
        .await
        .unwrap();
    fs::write(root.join("sub").join("nested").join("e.txt"), "e")
        .await
        .unwrap();
}

#[tokio::test]
async fn list_files_meta_recursive_flattens_all_files() {
    let tmp = tempfile::tempdir().unwrap();
    make_test_tree(tmp.path()).await;
    let cfg = default_test_config();
    let files = list_files_meta(tmp.path(), &cfg, None, None, true)
        .await
        .unwrap();
    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    // TS 0a7417f: 目录条目始终输出 (非空目录不再只铺开子项), DFS——
    // 目录条目后紧跟其子项; 根层目录在前 (sub 先于文件)
    assert_eq!(
        names,
        vec![
            "sub",
            "sub/nested",
            "sub/nested/e.txt",
            "sub/c.txt",
            "sub/d.log",
            "a.txt",
            "b.md"
        ]
    );
}

#[tokio::test]
async fn list_files_meta_single_level_lists_only_immediate_children() {
    let tmp = tempfile::tempdir().unwrap();
    make_test_tree(tmp.path()).await;
    let cfg = default_test_config();
    let files = list_files_meta(tmp.path(), &cfg, None, None, false)
        .await
        .unwrap();
    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    // 单层: 仅根目录直接子项
    assert!(names.contains(&"a.txt"));
    assert!(names.contains(&"b.md"));
    assert!(names.contains(&"sub")); // 子目录作为节点
    // 不应包含孙子层
    assert!(!names.contains(&"sub/c.txt"));
    assert!(!names.contains(&"sub/nested/e.txt"));
}

#[tokio::test]
async fn list_files_meta_depth_expands_levels_in_dfs_order() {
    // TS 1.5.4 depth: 单层模式 levels_left=2 时目录向下展开一层,
    // DFS 顺序 (目录条目后紧跟其子项); 层级用尽不再深入 (nested 子项不出现)。
    let tmp = tempfile::tempdir().unwrap();
    make_test_tree(tmp.path()).await;
    let cfg = default_test_config();
    let entries = list_files_meta_filtered(
        tmp.path(),
        &cfg,
        None,
        None,
        MetaListOptions {
            recursive: false,
            levels_left: 2,
            file_type: MetaListType::All,
            limit: None,
        },
    )
    .await
    .unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    // 根层排序目录在前 (sub 先于文件); DFS: sub 条目后紧跟其子项
    // (nested 目录在前, 再 c.txt/d.log); 层级用尽不再深入
    assert_eq!(
        names,
        vec![
            "sub",
            "sub/nested",
            "sub/c.txt",
            "sub/d.log",
            "a.txt",
            "b.md"
        ]
    );
}

#[tokio::test]
async fn list_files_meta_depth_with_file_type_still_descends_dirs() {
    // type=file 时目录条目不输出但**仍下钻** (深层文件取得到, 与 search 口径一致)
    let tmp = tempfile::tempdir().unwrap();
    make_test_tree(tmp.path()).await;
    let cfg = default_test_config();
    let entries = list_files_meta_filtered(
        tmp.path(),
        &cfg,
        None,
        None,
        MetaListOptions {
            recursive: false,
            levels_left: 2,
            file_type: MetaListType::File,
            limit: None,
        },
    )
    .await
    .unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    // sub 先处理 (不输出目录条目但下钻), 其子项后于根层文件之前
    assert_eq!(names, vec!["sub/c.txt", "sub/d.log", "a.txt", "b.md"]);
}

#[tokio::test]
async fn list_files_meta_depth_descent_stops_at_limit() {
    // 达 limit 后停止: 目录在前的根层序 sub 先 push(1), 下钻 nested(2),
    // c.txt(3) 推满即停; 外层与子层后续条目都不再输出
    let tmp = tempfile::tempdir().unwrap();
    make_test_tree(tmp.path()).await;
    let cfg = default_test_config();
    let entries = list_files_meta_filtered(
        tmp.path(),
        &cfg,
        None,
        None,
        MetaListOptions {
            recursive: false,
            levels_left: 2,
            file_type: MetaListType::All,
            limit: Some(3),
        },
    )
    .await
    .unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["sub", "sub/nested", "sub/c.txt"]);
}

#[tokio::test]
async fn list_files_meta_type_filter_lists_dirs_and_limits_output() {
    // TS 0a7417f: type=dir 递归输出全部目录条目 (含非空目录), 不再仅空目录
    let tmp = tempfile::tempdir().unwrap();
    make_test_tree(tmp.path()).await;
    fs::create_dir(tmp.path().join("empty")).await.unwrap();
    let cfg = default_test_config();

    let dir_options = MetaListOptions {
        recursive: true,
        levels_left: 1,
        file_type: MetaListType::Dir,
        limit: None,
    };
    let dirs = list_files_meta_filtered(tmp.path(), &cfg, None, None, dir_options)
        .await
        .unwrap();
    let names: Vec<&str> = dirs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["empty", "sub", "sub/nested"]);
    assert!(dirs.iter().all(|d| d.is_dir));

    let files = list_files_meta_filtered(
        tmp.path(),
        &cfg,
        None,
        None,
        MetaListOptions {
            file_type: MetaListType::File,
            limit: Some(1),
            ..dir_options
        },
    )
    .await
    .unwrap();
    assert_eq!(files.len(), 1);
    assert!(!files[0].is_dir);

    let none = list_files_meta_filtered(
        tmp.path(),
        &cfg,
        None,
        None,
        MetaListOptions {
            limit: Some(0),
            ..dir_options
        },
    )
    .await
    .unwrap();
    assert!(none.is_empty());
}

// ── TS e822516 escapesRoot 对齐: relativePath 指进越界目录链接 ────────────────

#[cfg(unix)]
#[tokio::test]
async fn list_hides_dangling_in_root_link() {
    // 对齐 TS 0a7417f isHiddenSymlink: 目标被删后悬空的界内链接不可见——
    // 断链打开必 404, 不进列表 (TS e822516 escapesRoot 的 fail-open 口径
    // 曾照常列出, 1.5.8 起列表收紧为隐藏); 界内实链接不受影响。
    // 单层/受限展开/递归三种模式一致。
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("real.txt"), "x").unwrap();
    std::fs::write(tmp.path().join("live-target.txt"), "y").unwrap();
    std::os::unix::fs::symlink("real.txt", tmp.path().join("dangling.txt")).unwrap();
    std::os::unix::fs::symlink("live-target.txt", tmp.path().join("live-link.txt")).unwrap();
    std::fs::remove_file(tmp.path().join("real.txt")).unwrap();
    let cfg = default_test_config();
    for (recursive, levels) in [(false, 1usize), (false, 2), (true, 1)] {
        let entries = list_files_meta_filtered(
            tmp.path(),
            &cfg,
            None,
            None,
            MetaListOptions {
                recursive,
                levels_left: levels,
                file_type: MetaListType::All,
                limit: None,
            },
        )
        .await
        .unwrap();
        let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        assert!(
            !names.iter().any(|n| n == "dangling.txt"),
            "recursive={recursive} levels={levels}: 悬空链接必须隐藏: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "live-link.txt"),
            "recursive={recursive} levels={levels}: 界内实链接必须列出: {names:?}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn metadata_preserves_posix_backslashes_in_paths_and_link_checks() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    fs::create_dir_all(root.join(r"sub\dir")).await.unwrap();
    fs::write(root.join("real.txt"), "inside").await.unwrap();
    fs::write(root.join(r"plain\file.txt"), "plain")
        .await
        .unwrap();
    fs::write(parent.path().join("outside.txt"), "outside")
        .await
        .unwrap();
    symlink("real.txt", root.join(r"live\link.txt")).unwrap();
    symlink("../real.txt", root.join(r"sub\dir/linked.txt")).unwrap();
    symlink("missing.txt", root.join(r"dangling\link.txt")).unwrap();
    symlink("../outside.txt", root.join(r"outside\link.txt")).unwrap();

    let config = default_test_config();
    for file_type in [MetaListType::All, MetaListType::File, MetaListType::Dir] {
        for (recursive, levels_left) in [(false, 1), (false, 2), (true, 1), (true, 2)] {
            let entries = list_files_meta_filtered(
                &root,
                &config,
                Some("/proxy"),
                None,
                MetaListOptions {
                    recursive,
                    levels_left,
                    file_type,
                    limit: None,
                },
            )
            .await
            .unwrap();
            let descends = recursive || levels_left > 1;
            let expected_links = if file_type == MetaListType::Dir {
                0
            } else if descends {
                2
            } else {
                1
            };
            assert_eq!(
                entries.iter().filter(|e| e.is_link == Some(true)).count(),
                expected_links,
                "type={file_type} recursive={recursive} depth={levels_left}: only live in-root links must be visible"
            );
            let mut expected_names = Vec::new();
            if file_type != MetaListType::File {
                expected_names.push(r"sub\dir");
            }
            if file_type != MetaListType::Dir {
                if descends {
                    expected_names.push(r"sub\dir/linked.txt");
                }
                expected_names.extend([r"live\link.txt", r"plain\file.txt", "real.txt"]);
            }
            let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, expected_names);
            for (name, url) in [
                (r"live\link.txt", "/proxy/live%5Clink.txt"),
                (r"plain\file.txt", "/proxy/plain%5Cfile.txt"),
                (r"sub\dir/linked.txt", "/proxy/sub%5Cdir/linked.txt"),
            ] {
                if expected_names.contains(&name) {
                    let entry = entries.iter().find(|e| e.name == name).unwrap();
                    assert_eq!(entry.file_proxy_url.as_deref(), Some(url));
                }
            }
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn list_files_meta_recursive_skips_unreadable_subtree() {
    // 对齐 TS 0a7417f: 单个子目录不可读 → 保留目录条目、WARN 跳过其子树,
    // 不拖垮整个请求 (此前错误上抛导致整个列表 500)
    let tmp = tempfile::tempdir().unwrap();
    make_test_tree(tmp.path()).await;
    let locked = tmp.path().join("locked");
    fs::create_dir(&locked).await.unwrap();
    fs::write(locked.join("secret.txt"), "s").await.unwrap();
    let mut perms = fs::metadata(&locked).await.unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o000);
    fs::set_permissions(&locked, perms).await.unwrap();

    let cfg = default_test_config();
    let result = list_files_meta_filtered(
        tmp.path(),
        &cfg,
        None,
        None,
        MetaListOptions {
            recursive: true,
            levels_left: 1,
            file_type: MetaListType::All,
            limit: None,
        },
    )
    .await;

    // 恢复权限保证 tempdir 清理, 再断言结果
    let mut perms = fs::metadata(&locked).await.unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&locked, perms).await.unwrap();

    let entries = result.unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"locked"), "目录条目必须保留: {names:?}");
    assert!(
        !names.contains(&"locked/secret.txt"),
        "不可读子树必须跳过: {names:?}"
    );
    assert!(names.contains(&"sub/c.txt"), "其余部分不受影响: {names:?}");
}

#[test]
fn traverse_exclude_dir_matching_follows_platform() {
    // 对齐 TS 0a7417f isTraverseExcludedDir: POSIX 精确匹配 (Dist 是合法的
    // 另一个目录); 仅 Windows 忽略大小写 (cfg 分支, macOS/Linux 走精确匹配)
    let dirs: Vec<String> = ["node_modules", "dist"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(is_traverse_excluded_dir(&dirs, "node_modules"));
    assert_eq!(
        is_traverse_excluded_dir(&dirs, "Node_Modules"),
        cfg!(windows)
    );
    assert!(is_traverse_excluded_dir(&dirs, "dist"));
    assert_eq!(is_traverse_excluded_dir(&dirs, "Dist"), cfg!(windows));
}

#[tokio::test]
async fn project_content_and_version_list_keep_case_distinct_excluded_dir() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join("Dist")).await.unwrap();
    fs::write(tmp.path().join("Dist").join("index.txt"), "included")
        .await
        .unwrap();
    fs::write(tmp.path().join("root.txt"), "root")
        .await
        .unwrap();
    let cfg = default_test_config();

    // project 链路本轮未同步 Windows 忽略大小写规则；版本内容列表复用同一遍历。
    let content = get_project_content(tmp.path(), &cfg, None, None)
        .await
        .unwrap();
    let version_files = list_files(tmp.path(), &cfg, None).await.unwrap();
    for files in [&content.files, &version_files] {
        let names: Vec<&str> = files.iter().map(|file| file.name.as_str()).collect();
        assert_eq!(names, vec!["Dist/index.txt", "root.txt"]);
        assert_eq!(files[0].contents.as_deref(), Some("included"));
        assert!(!files[0].is_dir);
    }

    // computer 列表 Windows 按忽略大小写排除 Dist；POSIX 保留其合法独立目录。
    for file_type in [MetaListType::All, MetaListType::File, MetaListType::Dir] {
        for (recursive, levels_left) in [(false, 1), (false, 2), (true, 1)] {
            let files = list_files_meta_filtered(
                tmp.path(),
                &cfg,
                None,
                None,
                MetaListOptions {
                    recursive,
                    levels_left,
                    file_type,
                    limit: None,
                },
            )
            .await
            .unwrap();
            let names: Vec<&str> = files.iter().map(|file| file.name.as_str()).collect();
            let includes_dist_dir = !cfg!(windows) && file_type != MetaListType::File;
            let includes_dist_file =
                !cfg!(windows) && file_type != MetaListType::Dir && (recursive || levels_left > 1);
            let mut expected_names = Vec::new();
            if includes_dist_dir {
                expected_names.push("Dist");
            }
            if includes_dist_file {
                expected_names.push("Dist/index.txt");
            }
            if file_type != MetaListType::Dir {
                expected_names.push("root.txt");
            }
            assert_eq!(
                names, expected_names,
                "recursive={recursive}, levels_left={levels_left}, type={file_type}"
            );
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn list_files_meta_relative_path_into_outside_dir_link_is_rejected() {
    // relativePath 指进目录符号链接 (link -> 根外目录) → 与字面 `..` 穿越
    // 同款 400 (message 英文, details.field/relativePath 键名对齐 TS);
    // 界内目录链接中段不受影响。
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("session");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub").join("a.txt"), "a").unwrap();
    std::fs::write(outer.path().join("secret.txt"), "outside").unwrap();
    std::os::unix::fs::symlink("sub", root.join("inside-dir")).unwrap();
    std::os::unix::fs::symlink("..", root.join("outside-dir")).unwrap();
    let cfg = default_test_config();
    let options = MetaListOptions {
        recursive: true,
        levels_left: 1,
        file_type: MetaListType::All,
        limit: None,
    };

    // 越界目录链接 → 400, 不泄露界外条目
    let Err(err) = list_files_meta_filtered(&root, &cfg, None, Some("outside-dir"), options).await
    else {
        panic!("越界目录链接作为列表起点必须 400");
    };
    let AppError::Validation(message, details) = &err else {
        panic!("必须是 Validation: {err:?}");
    };
    assert_eq!(
        message,
        "relativePath is not safe, cannot exceed target directory"
    );
    let details = details.as_ref().expect("details 必带字段回显");
    assert_eq!(details["field"], "relativePath");
    assert_eq!(details["relativePath"], "outside-dir");

    // 字面 `..` 穿越: 同一报错口径 (details 回显原始输入)
    let Err(err) = list_files_meta_filtered(&root, &cfg, None, Some("../outer"), options).await
    else {
        panic!("字面 .. 穿越必须 400");
    };
    assert!(matches!(err, AppError::Validation(..)), "{err:?}");

    // 界内目录链接中段: 正常列出
    let entries = list_files_meta_filtered(&root, &cfg, None, Some("inside-dir"), options)
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    // 界内目录链接中段: 正常列出。条目 name 前缀是**字面**列表起点
    // (relativePath=inside-dir → "inside-dir/a.txt", 与 TS 前缀回显一致,
    // 不替换为解析后的真实路径 sub/)
    assert_eq!(entries[0].name, "inside-dir/a.txt");
}

#[tokio::test]
async fn list_files_meta_with_relative_path_lists_subdir() {
    let tmp = tempfile::tempdir().unwrap();
    make_test_tree(tmp.path()).await;
    let cfg = default_test_config();
    // relative_path="sub" + recursive=false → 仅 sub 一层
    let files = list_files_meta(tmp.path(), &cfg, None, Some("sub"), false)
        .await
        .unwrap();
    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    assert!(names.contains(&"sub/c.txt"));
    assert!(names.contains(&"sub/d.log"));
    assert!(names.contains(&"sub/nested"));
    // 不含根层文件
    assert!(!names.contains(&"a.txt"));
}

#[tokio::test]
async fn list_files_meta_relative_path_traversal_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    make_test_tree(tmp.path()).await;
    let cfg = default_test_config();
    let err = list_files_meta(tmp.path(), &cfg, None, Some("../escape"), true)
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::Validation(..)));
}

#[tokio::test]
async fn list_files_meta_relative_path_not_directory_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    make_test_tree(tmp.path()).await;
    let cfg = default_test_config();
    // a.txt 是文件, 不是目录
    let err = list_files_meta(tmp.path(), &cfg, None, Some("a.txt"), true)
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::Validation(..)));
}

#[tokio::test]
async fn list_files_meta_proxy_url_encoded() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a b.txt"), "x").await.unwrap();
    let cfg = default_test_config();
    let files = list_files_meta(tmp.path(), &cfg, Some("/proxy"), None, false)
        .await
        .unwrap();
    let entry = files.iter().find(|f| f.name == "a b.txt").unwrap();
    // 空格应被 encode → %20
    assert_eq!(entry.file_proxy_url.as_deref(), Some("/proxy/a%20b.txt"));
}

#[cfg(unix)]
#[tokio::test]
async fn metadata_lists_only_symlinks_resolving_within_root() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    fs::create_dir(&root).await.unwrap();
    fs::write(root.join("target.txt"), "inside").await.unwrap();
    fs::write(parent.path().join("outside.txt"), "outside")
        .await
        .unwrap();
    symlink(root.join("target.txt"), root.join("inside-link.txt")).unwrap();
    symlink(
        parent.path().join("outside.txt"),
        root.join("outside-link.txt"),
    )
    .unwrap();

    let config = default_test_config();
    let entries = list_files_meta(&root, &config, Some("/proxy"), None, true)
        .await
        .unwrap();
    let inside_link = entries
        .iter()
        .find(|entry| entry.name == "inside-link.txt")
        .expect("in-root symlink should be listed");
    assert!(!inside_link.is_dir);
    assert_eq!(inside_link.is_link, Some(true));
    assert_eq!(
        inside_link.file_proxy_url.as_deref(),
        Some("/proxy/inside-link.txt")
    );
    assert!(
        !entries.iter().any(|entry| entry.name == "outside-link.txt"),
        "links resolving outside the selected root must stay out of the listing"
    );

    let content_entries = list_files(&root, &config, Some("/proxy")).await.unwrap();
    assert!(
        !content_entries
            .iter()
            .any(|entry| entry.name == "inside-link.txt"),
        "project content traversal must not follow symlinks"
    );
    assert!(
        !content_entries
            .iter()
            .any(|entry| entry.name == "outside-link.txt"),
        "project content traversal must not follow external symlinks"
    );
}

// ── resolve_subdir 单元测试 (components 语义, 不依赖文件系统) ──────────────

#[test]
fn resolve_subdir_none_and_empty_return_root() {
    let root = Path::new("/app/ws");
    assert_eq!(
        resolve_subdir(root, None).unwrap(),
        PathBuf::from("/app/ws")
    );
    assert_eq!(
        resolve_subdir(root, Some("")).unwrap(),
        PathBuf::from("/app/ws")
    );
    assert_eq!(
        resolve_subdir(root, Some("   ")).unwrap(),
        PathBuf::from("/app/ws")
    );
}

#[test]
fn resolve_subdir_dot_and_slash_return_root() {
    let root = Path::new("/app/ws");
    assert_eq!(
        resolve_subdir(root, Some(".")).unwrap(),
        PathBuf::from("/app/ws")
    );
    assert_eq!(
        resolve_subdir(root, Some("/")).unwrap(),
        PathBuf::from("/app/ws")
    );
}

#[test]
fn resolve_subdir_normal_path_resolved_under_root() {
    let root = Path::new("/app/ws");
    assert_eq!(
        resolve_subdir(root, Some("sub")).unwrap(),
        PathBuf::from("/app/ws/sub")
    );
    assert_eq!(
        resolve_subdir(root, Some("a/b/c")).unwrap(),
        PathBuf::from("/app/ws/a/b/c")
    );
}

#[test]
fn resolve_subdir_strips_leading_slash() {
    // 对齐 TS: "/sub" 应兼容为 root/sub (剥前导斜杠), 而非被当绝对路径拒绝
    let root = Path::new("/app/ws");
    assert_eq!(
        resolve_subdir(root, Some("/sub")).unwrap(),
        PathBuf::from("/app/ws/sub")
    );
    assert_eq!(
        resolve_subdir(root, Some("/a/b")).unwrap(),
        PathBuf::from("/app/ws/a/b")
    );
}

#[test]
fn resolve_subdir_cur_dir_skipped() {
    // "./sub" → CurDir 跳过, 只留 Normal("sub")
    let root = Path::new("/app/ws");
    assert_eq!(
        resolve_subdir(root, Some("./sub")).unwrap(),
        PathBuf::from("/app/ws/sub")
    );
    // "a/./b" → CurDir 跳过
    assert_eq!(
        resolve_subdir(root, Some("a/./b")).unwrap(),
        PathBuf::from("/app/ws/a/b")
    );
}

#[test]
fn resolve_subdir_parent_dir_cancels_normal() {
    // a/../b → .. 抵消 a, 归约为 b (合法, 对齐 TS path.normalize)
    let root = Path::new("/app/ws");
    assert_eq!(
        resolve_subdir(root, Some("a/../b")).unwrap(),
        PathBuf::from("/app/ws/b")
    );
    // a/./../b → . 跳过, .. 抵消 a → b
    assert_eq!(
        resolve_subdir(root, Some("a/./../b")).unwrap(),
        PathBuf::from("/app/ws/b")
    );
    // ./.. → CurDir 跳过, ParentDir 栈空 → 越界拒绝
    assert!(resolve_subdir(root, Some("./..")).is_err());
}

#[test]
fn resolve_subdir_parent_dir_overflow_rejected() {
    // .. 超过前面的 Normal 段数 = 越界 (栈空仍要 pop)
    let root = Path::new("/app/ws");
    assert!(resolve_subdir(root, Some("../escape")).is_err());
    assert!(resolve_subdir(root, Some("a/../../escape")).is_err());
    // a/../b/../../x: 抵消后 b 还剩, 再 .. 栈空 → 越界
    assert!(resolve_subdir(root, Some("a/../b/../../x")).is_err());
}

#[test]
fn strip_leading_root_components_uses_host_component_semantics() {
    assert_eq!(strip_leading_root_components("src/a.md"), "src/a.md");
    assert_eq!(strip_leading_root_components("/src/a.md"), "src/a.md");
    assert_eq!(strip_leading_root_components("//src"), "src");
    assert_eq!(strip_leading_root_components("/"), "");
    // 宿主语义（有意偏离 TS 的字符集剥法）：POSIX 上 `\` 是普通文件名字符，
    // 组件层不剥；Windows 上是分隔符，照剥
    #[cfg(not(windows))]
    assert_eq!(strip_leading_root_components("\\src"), "\\src");
    #[cfg(windows)]
    assert_eq!(strip_leading_root_components("\\src"), "src");
}

#[test]
fn resolve_subdir_keeps_edge_whitespace_in_segments() {
    // 有意偏离 TS：不做整体 trim——首尾空格是合法路径段的一部分
    let root = Path::new("/app/ws");
    assert_eq!(
        resolve_subdir(root, Some(" dir ")).unwrap(),
        PathBuf::from("/app/ws/ dir ")
    );
    // 中段空格不受影响（原本就原样）
    assert_eq!(
        resolve_subdir(root, Some("/ lead/file.txt")).unwrap(),
        PathBuf::from("/app/ws/ lead/file.txt")
    );
}

/// P2/FS-08 反例: POSIX 下 `a\b.txt`（单文件名）与 `a/b.txt`（子目录文件）是两个
/// 不同对象——project 内容入口此前无条件替换反斜杠使两者同名碰撞; 修复后各自无损,
/// URL 逐段编码后可分别访问（`#`/`?` 不再变成 query/fragment）。
#[cfg(unix)]
#[tokio::test]
async fn project_content_preserves_posix_backslash_and_encodes_url() {
    use super::content::{get_project_content, list_files};
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::write(root.join("a\\b.txt"), "BACKSLASH-CONTENT").unwrap();
    std::fs::write(root.join("a/b.txt"), "SLASH-CONTENT").unwrap();
    // 带 URL 敏感字符的文件名
    std::fs::write(root.join("weird#name?x.txt"), "WEIRD").unwrap();
    let cfg = Config::default();

    let files = list_files(root, &cfg, None).await.unwrap();
    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    assert!(
        names.contains(&"a\\b.txt"),
        "backslash filename must stay verbatim: {names:?}"
    );
    assert!(
        names.contains(&"a/b.txt"),
        "slash path must be intact: {names:?}"
    );
    assert_eq!(
        names
            .iter()
            .filter(|n| **n == "a/b.txt" || **n == "a\\b.txt")
            .count(),
        2,
        "two distinct objects must not collide into one name"
    );

    // URL: 逐段编码——反斜杠、#、? 各自转义
    let with_proxy = list_files(root, &cfg, Some("/proxy")).await.unwrap();
    for entry in &with_proxy {
        if let Some(url) = &entry.file_proxy_url {
            if entry.name == "a\\b.txt" {
                assert!(
                    url.contains("a%5Cb.txt"),
                    "backslash must be percent-encoded: {url}"
                );
            }
            if entry.name.starts_with("weird") {
                assert!(
                    url.contains("weird%23name%3Fx"),
                    "# and ? must be encoded: {url}"
                );
            }
        }
    }

    // get_project_content: 同一清单语义（含内容读取路径不因反斜杠走错对象）
    let content = get_project_content(root, &cfg, None, None).await.unwrap();
    let backslash = content
        .files
        .iter()
        .find(|f| f.name == "a\\b.txt")
        .expect("backslash entry");
    assert_eq!(
        backslash.contents.as_deref(),
        Some("BACKSLASH-CONTENT"),
        "content must belong to the actual backslash-named file"
    );
    let slash = content
        .files
        .iter()
        .find(|f| f.name == "a/b.txt")
        .expect("slash entry");
    assert_eq!(slash.contents.as_deref(), Some("SLASH-CONTENT"));
}

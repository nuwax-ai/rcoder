use super::types::Side;
use super::*;
use crate::service::git::{commit_indexed, init_repo, stage_path};
use gix::objs::tree::EntryKind;
use gix::open;

#[test]
fn commit_diff_handles_nested_tree_and_preserves_from_to_direction() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "file-server-diff-test-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(root.join("src")).expect("create fixture");
    init_repo(&root, "Test", "test@example.com").expect("init repo");
    let repo = open(&root).expect("open repo");

    std::fs::write(root.join("src/app.txt"), "old\n").expect("write old");
    stage_path(&repo, "src/app.txt").expect("stage old");
    let old = commit_indexed(&repo, "old", "Test", "test@example.com").expect("commit old");

    std::fs::write(root.join("src/app.txt"), "new\n").expect("write new");
    std::fs::write(root.join("src/added.txt"), "added\n").expect("write added");
    stage_path(&repo, "src").expect("stage nested tree");
    let new = commit_indexed(&repo, "new", "Test", "test@example.com").expect("commit new");

    let result = compute_diff(
        &repo,
        &DiffParams {
            source: DiffSource::Commit,
            from: Some(old),
            to: Some(new.clone()),
            paths: Vec::new(),
            max_file_size_bytes: 16 * 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024,
            max_output_bytes: 64 * 1024 * 1024,
        },
    )
    .expect("explicit commit diff");
    assert_eq!(
        result
            .files
            .iter()
            .map(|file| file.file.as_str())
            .collect::<Vec<_>>(),
        ["src/added.txt", "src/app.txt"]
    );
    assert!(result.diff.contains("-old"));
    assert!(result.diff.contains("+new"));

    let from_only = compute_diff(
        &repo,
        &DiffParams {
            source: DiffSource::Commit,
            from: Some(new),
            to: None,
            paths: Vec::new(),
            max_file_size_bytes: 16 * 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024,
            max_output_bytes: 64 * 1024 * 1024,
        },
    )
    .expect("from-only commit diff");
    assert_eq!(from_only.insertions, result.insertions);
    assert_eq!(from_only.deletions, result.deletions);

    std::fs::write(root.join("src/app.txt"), "content beyond tiny limit\n")
        .expect("write oversized diff fixture");
    let oversized = compute_diff(
        &repo,
        &DiffParams {
            source: DiffSource::Worktree,
            from: None,
            to: None,
            paths: Vec::new(),
            max_file_size_bytes: 1,
            max_total_bytes: 2,
            max_output_bytes: 64 * 1024 * 1024,
        },
    );
    assert!(oversized.is_err());

    drop(repo);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn path_filter_skips_unselected_oversized_changes() {
    let directory = tempfile::tempdir().expect("create test directory");
    let root = directory.path();
    init_repo(root, "Test", "test@example.com").expect("init repo");
    let repo = open(root).expect("open repo");

    std::fs::write(root.join("small.txt"), "a\n").expect("write small fixture");
    std::fs::write(root.join("large.txt"), vec![b'x'; 128]).expect("write large fixture");
    stage_path(&repo, ".").expect("stage fixtures");
    commit_indexed(&repo, "fixtures", "Test", "test@example.com").expect("commit fixtures");

    std::fs::write(root.join("small.txt"), "b\n").expect("modify small fixture");
    std::fs::write(root.join("large.txt"), vec![b'y'; 128]).expect("modify large fixture");
    let result = compute_diff(
        &repo,
        &DiffParams {
            source: DiffSource::Worktree,
            from: None,
            to: None,
            paths: vec!["small.txt".to_string()],
            max_file_size_bytes: 8,
            max_total_bytes: 16,
            max_output_bytes: 1024 * 1024,
        },
    )
    .expect("filter before loading oversized file");

    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].file, "small.txt");
    assert!(!result.diff.contains("large.txt"));
}

#[test]
fn binary_addition_and_deletion_use_dev_null_without_text_markers() {
    let directory = tempfile::tempdir().expect("create test directory");
    init_repo(directory.path(), "Test", "test@example.com").expect("init repo");
    let repo = open(directory.path()).expect("open repo");
    let mode = EntryKind::Blob.into();
    let added_bytes = vec![0, 1, 2];
    let deleted_bytes = vec![0, 3, 4];
    let added_id = gix::objs::compute_hash(repo.object_hash(), gix::objs::Kind::Blob, &added_bytes)
        .expect("hash added fixture");
    let deleted_id =
        gix::objs::compute_hash(repo.object_hash(), gix::objs::Kind::Blob, &deleted_bytes)
            .expect("hash deleted fixture");

    let result = render_changes(
        &repo,
        vec![
            FileChange {
                path: "added.bin".to_string(),
                old: Side::missing(),
                new: Side::present(added_bytes, mode),
            },
            FileChange {
                path: "deleted.bin".to_string(),
                old: Side::present(deleted_bytes, mode),
                new: Side::missing(),
            },
        ],
        1024 * 1024,
    )
    .expect("render binary changes");

    assert!(
        result
            .diff
            .contains("Binary files /dev/null and b/added.bin differ")
    );
    assert!(
        result
            .diff
            .contains("Binary files a/deleted.bin and /dev/null differ")
    );
    assert!(!result.diff.lines().any(|line| line.starts_with("--- ")));
    assert!(!result.diff.lines().any(|line| line.starts_with("+++ ")));
    assert!(repo.find_object(added_id).is_err());
    assert!(repo.find_object(deleted_id).is_err());
}

#[test]
fn mode_only_change_is_not_dropped() {
    let old = Side::present(b"same\n".to_vec(), EntryKind::Blob.into());
    let new = Side::present(b"same\n".to_vec(), EntryKind::BlobExecutable.into());
    let header = assemble_header("script.sh", &old, &new, "1111111", "1111111", false, false)
        .expect("render mode-only header");

    assert_eq!(
        header,
        "diff --git a/script.sh b/script.sh\nold mode 100644\nnew mode 100755\n"
    );
}

#[cfg(unix)]
#[test]
fn worktree_symlink_reads_link_target_without_following_it() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().expect("create test directory");
    let outside = directory.path().join("outside-secret.txt");
    std::fs::write(&outside, "secret contents").expect("write outside fixture");
    let worktree = directory.path().join("worktree");
    std::fs::create_dir(&worktree).expect("create worktree fixture");
    symlink(&outside, worktree.join("link")).expect("create symlink fixture");

    let side = read_worktree_file(&worktree, "link", 4096).expect("read symlink side");
    assert_eq!(
        side.bytes.expect("symlink bytes"),
        outside.as_os_str().as_bytes()
    );
    assert_eq!(side.mode, Some(EntryKind::Link.into()));
}

#[test]
fn hunk_writer_marks_missing_trailing_newlines_and_counts_changes() {
    let rendered =
        render_blob_diff(Some(b"old"), Some(b"new")).expect("render missing-newline fixture");

    assert_eq!(rendered.insertions, 1);
    assert_eq!(rendered.deletions, 1);
    assert_eq!(
        rendered
            .hunks
            .matches(r"\ No newline at end of file")
            .count(),
        2
    );
}

#[test]
fn rendered_output_has_an_independent_hard_limit() {
    let directory = tempfile::tempdir().expect("create test directory");
    init_repo(directory.path(), "Test", "test@example.com").expect("init repo");
    let repo = open(directory.path()).expect("open repo");
    let result = render_changes(
        &repo,
        vec![FileChange {
            path: "limited.txt".to_string(),
            old: Side::present(b"old\n".to_vec(), EntryKind::Blob.into()),
            new: Side::present(b"new\n".to_vec(), EntryKind::Blob.into()),
        }],
        8,
    );

    assert!(result.is_err());
}

#[test]
fn diff_source_has_canonical_string_roundtrip() {
    for (input, expected) in [
        ("", DiffSource::Worktree),
        ("worktree", DiffSource::Worktree),
        ("staged", DiffSource::Staged),
        ("commit", DiffSource::Commit),
    ] {
        let parsed = input.parse::<DiffSource>().expect("parse diff source");
        assert_eq!(parsed, expected);
        assert_eq!(parsed.to_string(), expected.to_string());
    }
}

//! Git tree entry 安全物化器（FS-03/FS-06）。
//!
//! checkout/reset/revert/switch 与 discard 恢复的唯一物化路径：
//! - **整批预检**：词法界内 + mode 支持（gitlink 整批拒绝），通过前不产生任何
//!   副作用，不支持的对象不会留下半写的工作树；
//! - **目录段逐级校验**：已存在段必须是真实目录，或解析后仍在工作区内的链接
//!   （保留合法界内目录链接能力）；外向目录链接在写入前拒绝；
//! - **leaf 不跟随**：已有链接先删除链接本身再物化（git checkout 覆盖语义），
//!   新文件以 `O_NOFOLLOW` 创建——预检与写入之间被替换的链接不会被跟随写出界；
//! - **mode 分派**：普通 / 可执行（保留执行位）/ symlink（如实恢复链接对象，
//!   含外向目标——链接内容是用户提交过的数据，写入侧从不跟随它）。

use std::path::{Path, PathBuf};

use gix::index::entry::Mode as IndexMode;

use crate::error::{AppError, AppResult};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EntryKind {
    File,
    Executable,
    Symlink,
}

impl EntryKind {
    fn from_index_mode(mode: IndexMode) -> Option<Self> {
        match mode {
            IndexMode::FILE => Some(Self::File),
            IndexMode::FILE_EXECUTABLE => Some(Self::Executable),
            IndexMode::SYMLINK => Some(Self::Symlink),
            _ => None,
        }
    }

    fn from_tree_mode(mode: gix::object::tree::EntryMode) -> Option<Self> {
        if mode.is_executable() {
            Some(Self::Executable)
        } else if mode.is_link() {
            Some(Self::Symlink)
        } else if mode.is_blob() {
            Some(Self::File)
        } else {
            // Tree / gitlink（submodule）不在物化范围
            None
        }
    }
}

/// 预检通过的单个待物化 entry（无副作用；blob 内容在物化时才读取）。
pub(crate) struct PlannedEntry {
    pub rel: PathBuf,
    pub dest: PathBuf,
    pub kind: EntryKind,
    pub blob_id: gix::ObjectId,
}

/// 词法预检 index entry：路径界内 + mode 支持。gitlink 等未承诺对象在此
/// 拒绝，调用方应先对全部 entries 完成预检再逐个物化（整批语义）。
pub(crate) fn plan_index_entry(
    workdir: &Path,
    rel: &Path,
    mode: IndexMode,
    blob_id: gix::ObjectId,
) -> AppResult<PlannedEntry> {
    let Some(kind) = EntryKind::from_index_mode(mode) else {
        return Err(AppError::business(format!(
            "git entry '{}' has unsupported mode {mode:?} (gitlink/submodule entries are not supported); refusing partial materialization",
            rel.display()
        )));
    };
    let dest = crate::path_safety::ensure_within_path(workdir, rel)?;
    Ok(PlannedEntry {
        rel: rel.to_path_buf(),
        dest,
        kind,
        blob_id,
    })
}

/// 预检 tree entry（discard 单文件恢复用）。
pub(crate) fn plan_tree_entry(
    workdir: &Path,
    rel: &Path,
    mode: gix::object::tree::EntryMode,
    id: gix::Id<'_>,
) -> AppResult<PlannedEntry> {
    let Some(kind) = EntryKind::from_tree_mode(mode) else {
        return Err(AppError::business(format!(
            "git entry '{}' has unsupported kind (gitlink/submodule entries are not supported)",
            rel.display()
        )));
    };
    let dest = crate::path_safety::ensure_within_path(workdir, rel)?;
    Ok(PlannedEntry {
        rel: rel.to_path_buf(),
        dest,
        kind,
        blob_id: id.detach(),
    })
}

/// 物化一个预检 entry：读取 blob 并按 mode 落盘。
pub(crate) fn materialize_planned(
    repo: &gix::Repository,
    workdir: &Path,
    planned: &PlannedEntry,
) -> AppResult<()> {
    let blob = repo
        .find_blob(planned.blob_id)
        .map_err(|e| super::map_git_err(e, "git find_blob (materialize)"))?;
    materialize_bytes(
        workdir,
        &planned.rel,
        &planned.dest,
        planned.kind,
        &blob.data,
    )
}

/// 按 kind 物化 blob 字节（repo 无关，供测试与计划物化共用）。
pub(crate) fn materialize_bytes(
    workdir: &Path,
    rel: &Path,
    dest: &Path,
    kind: EntryKind,
    data: &[u8],
) -> AppResult<()> {
    ensure_real_dir_chain(workdir, rel)?;
    match std::fs::symlink_metadata(dest) {
        Ok(meta) if meta.file_type().is_symlink() => {
            std::fs::remove_file(dest).map_err(|e| {
                AppError::system(format!("remove existing link {}: {e}", dest.display()))
            })?;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(AppError::system(format!(
                "inspect {}: {error}",
                dest.display()
            )));
        }
    }
    match kind {
        EntryKind::Symlink => {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                let target = <std::ffi::OsStr as OsStrExt>::from_bytes(data);
                std::os::unix::fs::symlink(target, dest).map_err(|e| {
                    AppError::system(format!("create link {}: {e}", dest.display()))
                })?;
            }
            #[cfg(not(unix))]
            {
                let _ = data;
                return Err(AppError::business(
                    "symlink entries require a Unix filesystem",
                ));
            }
        }
        EntryKind::File | EntryKind::Executable => {
            write_leaf_nofollow(dest, data)?;
            if kind == EntryKind::Executable {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))
                        .map_err(|e| {
                            AppError::system(format!("set executable bit {}: {e}", dest.display()))
                        })?;
                }
                #[cfg(not(unix))]
                {
                    // Windows 无执行位概念: 按普通文件物化（如实声明, 不伪造）。
                }
            }
        }
    }
    Ok(())
}

/// 逐级确保 `rel` 的中间目录段（不含 leaf）为真实目录或**界内**链接。
/// 界内判定基准是 workdir 的 canonical 根；workdir 本身允许是调用方
/// （resolver）选择的链接（与 path_safety 的根信任语义一致）。
fn ensure_real_dir_chain(workdir: &Path, rel: &Path) -> AppResult<()> {
    let Some(parent) = rel.parent() else {
        return Ok(());
    };
    if parent == Path::new("") {
        return Ok(()); // 顶层文件无中间目录段
    }
    let root = std::fs::canonicalize(workdir)
        .map_err(|e| AppError::system(format!("resolve workdir {}: {e}", workdir.display())))?;
    let mut current = workdir.to_path_buf();
    for component in parent.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(AppError::validation(format!(
                "git entry '{}' has an unexpected path component",
                rel.display()
            )));
        };
        current.push(name);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.is_dir() => {}
            Ok(meta) if meta.is_symlink() => {
                let resolved = std::fs::canonicalize(&current)
                    .map_err(|e| AppError::system(format!("resolve {}: {e}", current.display())))?;
                if !resolved.starts_with(&root) {
                    return Err(AppError::validation(format!(
                        "git entry '{}' escapes the workspace through directory link '{}'",
                        rel.display(),
                        current.display()
                    )));
                }
                // 界内链接：后续段在其下继续逐级校验。
            }
            Ok(_) => {
                return Err(AppError::validation(format!(
                    "git entry '{}' crosses a non-directory entry '{}'",
                    rel.display(),
                    current.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current).map_err(|e| {
                    AppError::system(format!("create dir {}: {e}", current.display()))
                })?;
            }
            Err(error) => {
                return Err(AppError::system(format!(
                    "inspect {}: {error}",
                    current.display()
                )));
            }
        }
    }
    Ok(())
}

/// leaf 写入：`O_NOFOLLOW` 创建/截断（Unix）。dest 若在预检后被换成链接，
/// 这里得到 ELOOP 而不是跟随写出工作区。与 UserApp 上传共用同一原语。
fn write_leaf_nofollow(dest: &Path, data: &[u8]) -> AppResult<()> {
    crate::path_safety::write_file_nofollow_blocking(dest, data)
        .map_err(|e| AppError::system(format!("create {}: {e}", dest.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FS-03 反例: 工作区已有外向目录链接时, 物化不得沿链接写出界。
    /// 修复前 `create_dir_all` + `fs::write` 跟随链接写外部文件。
    #[cfg(unix)]
    #[test]
    fn materialize_refuses_outward_directory_link() {
        let fixture = tempfile::tempdir().expect("fixture");
        let workdir = fixture.path().join("work");
        std::fs::create_dir_all(&workdir).expect("workdir");
        let outside = fixture.path().join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(outside.join("sentinel"), b"external").expect("sentinel");
        std::os::unix::fs::symlink(&outside, workdir.join("sub")).expect("outward link");

        let rel = Path::new("sub/file.txt");
        let dest = workdir.join(rel);
        let error = materialize_bytes(&workdir, rel, &dest, EntryKind::File, b"tree content")
            .expect_err("outward directory link must be rejected");
        assert!(
            matches!(error, AppError::Validation(..)),
            "expected validation error, got: {error:?}"
        );
        assert!(
            !outside.join("file.txt").exists(),
            "external file must not be created through the link"
        );
        assert_eq!(
            std::fs::read(outside.join("sentinel")).expect("sentinel intact"),
            b"external"
        );
        // 界内文件物化不受影响
        materialize_bytes(
            &workdir,
            Path::new("keep.txt"),
            &workdir.join("keep.txt"),
            EntryKind::File,
            b"kept",
        )
        .expect("in-workspace file materializes");
        assert_eq!(
            std::fs::read(workdir.join("keep.txt")).expect("kept"),
            b"kept"
        );
    }

    /// FS-03 反例（leaf 链接变体）: 目标位置已是外向文件链接时, 物化按
    /// checkout 覆盖语义替换链接本身, 不跟随改写外部目标。
    #[cfg(unix)]
    #[test]
    fn materialize_replaces_leaf_symlink_without_following() {
        let fixture = tempfile::tempdir().expect("fixture");
        let workdir = fixture.path().join("work");
        std::fs::create_dir_all(&workdir).expect("workdir");
        let external = fixture.path().join("external.txt");
        std::fs::write(&external, b"external-original").expect("external");
        std::os::unix::fs::symlink(&external, workdir.join("target.txt")).expect("leaf link");

        materialize_bytes(
            &workdir,
            Path::new("target.txt"),
            &workdir.join("target.txt"),
            EntryKind::File,
            b"tree content",
        )
        .expect("materialize replaces the link");

        assert!(
            std::fs::symlink_metadata(workdir.join("target.txt"))
                .expect("replaced entry")
                .is_file(),
            "leaf must now be a regular file"
        );
        assert_eq!(
            std::fs::read(workdir.join("target.txt")).expect("tree content"),
            b"tree content"
        );
        assert_eq!(
            std::fs::read(&external).expect("external untouched"),
            b"external-original"
        );
    }

    /// FS-06 反例: 可执行位与链接对象必须如实物化（修复前一律普通文本文件）。
    #[cfg(unix)]
    #[test]
    fn materialize_restores_executable_and_symlink_modes() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir().expect("fixture");
        let workdir = fixture.path().join("work");
        std::fs::create_dir_all(workdir.join("nested")).expect("dirs");

        materialize_bytes(
            &workdir,
            Path::new("nested/run.sh"),
            &workdir.join("nested/run.sh"),
            EntryKind::Executable,
            b"#!/bin/sh\n",
        )
        .expect("executable");
        let mode = std::fs::metadata(workdir.join("nested/run.sh"))
            .expect("stat")
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "executable bit must be restored");

        materialize_bytes(
            &workdir,
            Path::new("nested/alias"),
            &workdir.join("nested/alias"),
            EntryKind::Symlink,
            b"run.sh",
        )
        .expect("symlink");
        let meta = std::fs::symlink_metadata(workdir.join("nested/alias")).expect("lstat");
        assert!(
            meta.file_type().is_symlink(),
            "link object must be restored"
        );
        assert_eq!(
            std::fs::read_link(workdir.join("nested/alias")).expect("target"),
            Path::new("run.sh")
        );
    }

    /// FS-06/整批预检: gitlink（submodule）entry 在计划阶段拒绝, 不产生半写。
    #[test]
    fn plan_rejects_gitlink_entry() {
        let workdir = Path::new("/tmp/w");
        let null_id = gix::hash::Kind::Sha1.null();
        let result = plan_index_entry(workdir, Path::new("vendor/lib"), IndexMode::COMMIT, null_id);
        let error = match result {
            Ok(_) => panic!("gitlink must be rejected at plan time"),
            Err(error) => error,
        };
        assert!(matches!(error, AppError::Business(_)), "{error:?}");
    }
}

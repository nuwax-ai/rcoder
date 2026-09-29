use std::io::Read;
use std::path::{Path, PathBuf};

use gix::hash::ObjectId;
use gix::index::{File as IndexFile, entry::Stage as IndexStage};
use gix::objs::tree::{EntryKind, EntryMode};
use gix::path::into_bstr;
use gix::{Repository, Tree};

use crate::error::{AppError, AppResult};

use super::super::map_git_err;
use super::types::Side;

pub(super) fn read_blob(
    repo: &Repository,
    path: &str,
    id: Option<ObjectId>,
    mode: Option<EntryMode>,
    max_bytes: u64,
) -> AppResult<Side> {
    match (id, mode) {
        (Some(id), Some(mode)) => {
            // 超限降级（对齐 TS git CLI 的 Binary files differ 语义）：
            // 单文件超限不终止整个 diff，warn 后按 binary 标记渲染。
            let size = repo
                .find_header(id)
                .map_err(|error| map_git_err(error, "git find blob header"))?
                .size();
            if size > max_bytes {
                tracing::warn!(
                    path,
                    size,
                    limit = max_bytes,
                    "git diff blob exceeds per-file limit; degraded to binary marker"
                );
                return Ok(Side::present_oversized(mode, id));
            }
            let blob = repo
                .find_blob(id)
                .map_err(|error| map_git_err(error, "git find_blob"))?;
            Ok(Side::present_blob(blob.data.to_vec(), mode, id))
        }
        (None, None) => Ok(Side::missing()),
        _ => Err(AppError::system(
            "git diff side has inconsistent object metadata",
        )),
    }
}

pub(super) fn head_tree(repo: &Repository) -> AppResult<Tree<'_>> {
    let head_id = repo
        .head_tree_id_or_empty()
        .map_err(|e| map_git_err(e, "git head_tree_id_or_empty"))?;
    repo.find_tree(head_id)
        .map_err(|e| map_git_err(e, "git find_tree (head)"))
}

pub(super) fn read_head_blob(
    repo: &Repository,
    tree: &Tree<'_>,
    path: &str,
    max_bytes: u64,
) -> AppResult<Side> {
    let entry = tree
        .lookup_entry_by_path(path)
        .map_err(|error| map_git_err(error, "git lookup_entry_by_path"))?;
    match entry {
        Some(entry) => read_blob(
            repo,
            path,
            Some(entry.id().detach()),
            Some(entry.mode()),
            max_bytes,
        ),
        None => Ok(Side::missing()),
    }
}

pub(super) fn read_index_blob(
    repo: &Repository,
    index: &IndexFile,
    path: &str,
    max_bytes: u64,
) -> AppResult<Side> {
    let bstr_path = into_bstr(PathBuf::from(path));
    let Some(entry) = index.entry_by_path_and_stage(bstr_path.as_ref(), IndexStage::Unconflicted)
    else {
        return Ok(Side::missing());
    };
    let mode = entry
        .mode
        .to_tree_entry_mode()
        .ok_or_else(|| AppError::system(format!("unsupported git index mode for {path}")))?;
    read_blob(repo, path, Some(entry.id), Some(mode), max_bytes)
}

pub(super) fn read_worktree_file(
    workdir: &Path,
    path: &str,
    max_bytes: u64,
    hash_kind: gix::hash::Kind,
) -> AppResult<Side> {
    let full_path = workdir.join(path);
    let metadata = match std::fs::symlink_metadata(&full_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Side::missing()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(&full_path)?;
        let bytes = symlink_target_bytes(&target);
        if u64::try_from(bytes.len()).map_or(true, |size| size > max_bytes) {
            let id = gix::objs::compute_hash(hash_kind, gix::objs::Kind::Blob, &bytes)
                .map_err(|error| map_git_err(error, "git hash symlink target"))?;
            return Ok(Side::present_oversized(EntryKind::Link.into(), id));
        }
        return Ok(Side::present(bytes, EntryKind::Link.into()));
    }
    if !metadata.is_file() {
        return Err(AppError::validation(format!(
            "git diff path is not a regular file or symlink: {}",
            full_path.display()
        )));
    }
    let mut file = std::fs::File::open(&full_path)?;
    let metadata = file.metadata()?;
    let size_bytes = metadata.len();
    if size_bytes > max_bytes {
        // gix 用固定大小缓冲计算 Git blob ID，不加载整个文件、不写对象库。
        // 候选可能只在 index 改变，HEAD 与工作区相同，不能凭超限虚报变更。
        let id = gix::objs::compute_stream_hash(
            hash_kind,
            gix::objs::Kind::Blob,
            &mut file,
            size_bytes,
            &mut gix::progress::Discard,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .map_err(|error| map_git_err(error, "git hash oversized worktree file"))?;
        if file.read(&mut [0_u8; 1])? != 0 {
            return Err(AppError::system(format!(
                "git diff file changed while reading: {}",
                full_path.display()
            )));
        }
        tracing::warn!(
            path = %full_path.display(),
            size = size_bytes,
            limit = max_bytes,
            "git diff worktree file exceeds per-file limit; degraded to binary marker"
        );
        return Ok(Side::present_oversized(regular_file_mode(&metadata), id));
    }
    // 即使文件在 stat 后增长，也不允许无界分配；请调用方重新观察新版本。
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).ok() != Some(size_bytes) {
        return Err(AppError::system(format!(
            "git diff file changed while reading: {}",
            full_path.display()
        )));
    }
    Ok(Side::present(bytes, regular_file_mode(&metadata)))
}

#[cfg(unix)]
fn symlink_target_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn symlink_target_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

fn regular_file_mode(_metadata: &std::fs::Metadata) -> EntryMode {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if _metadata.permissions().mode() & 0o111 != 0 {
            return EntryKind::BlobExecutable.into();
        }
    }
    EntryKind::Blob.into()
}

//! Git 写操作 (stage/commit)。

use std::collections::HashSet;
use std::path::Path;

use gix::Repository;
use gix::actor::Signature;
use gix::bstr::{BStr, BString};
use gix::date::{Time, parse::TimeBuf};
use gix::hash::ObjectId;
use gix::index::{
    entry::{Flags, Mode as IndexMode, Stage, Stat},
    write::Options as IndexWriteOptions,
};
use gix::object::tree::EntryKind as TreeEntryKind;
use gix::objs::Write;
use gix::path::into_bstr;

use crate::error::{AppError, AppResult};

use super::map_git_err;
use super::read::get_status;

/// 工作区相对路径 → git index/tree 条目名。
///
/// git 树条目名**必须用 `/`**：gix 的 tree editor 会拒绝含 `\` 的文件名
/// （"has an invalid filename: 'src\added.txt'"）。Windows 上遍历得到的宿主
/// 相对路径带 `\`，故在此按 FS-08 同一口径归一
/// （`path_safety::host_relative_to_wire`；POSIX 下恒等，反斜杠文件名不受影响）。
///
/// 仅用于 index/tree 名，**不做文件系统访问**——文件系统侧仍用原始路径
/// （Win32 两种分隔符都接受）。
fn index_bstr_path(worktree_path: &str) -> AppResult<BString> {
    let normalized = crate::path_safety::host_relative_to_wire(worktree_path);
    into_bstr(std::path::PathBuf::from(normalized))
        .map(|path| path.into_owned())
        .map_err(|error| map_git_err(error, "git encode worktree index path"))
}

/// stage 单个路径: 文件存在 → 更新/新增 index entry; 不存在 → stage 删除 (对齐 nuwax stageFiles)。
pub fn stage_path(repo: &Repository, worktree_path: &str) -> AppResult<()> {
    let mut paths = Vec::new();
    expand_stage_paths(repo, worktree_path, &mut paths)?;
    // 空目录展开为空 → 无操作; 不能落入 stage_files 的空列表 addAll 语义
    if paths.is_empty() {
        return Ok(());
    }
    stage_files(repo, &paths)
}

/// 路径展开: 目录 → 递归收集内部文件 (跳过 .git); 文件/不存在 → 原样收集
/// (不存在后续按 stage 删除语义处理, 对齐原 stage_path 行为)。
fn expand_stage_paths(
    repo: &Repository,
    worktree_path: &str,
    out: &mut Vec<String>,
) -> AppResult<()> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| AppError::system("git repo has no workdir"))?;
    let abs = crate::path_safety::ensure_within(workdir, worktree_path)?;
    if std::fs::symlink_metadata(&abs).is_ok_and(|metadata| metadata.is_dir()) {
        collect_stage_paths(workdir, &abs, out)?;
        return Ok(());
    }
    out.push(worktree_path.to_string());
    Ok(())
}

fn collect_stage_paths(workdir: &Path, dir: &Path, out: &mut Vec<String>) -> AppResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if entry.file_name() != ".git" {
                collect_stage_paths(workdir, &path, out)?;
            }
        } else {
            let relative = path
                .strip_prefix(workdir)
                .map_err(|_| AppError::validation("git path is outside worktree"))?;
            out.push(relative.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

/// stage 多个文件 (对齐 nuwax stageFiles; files 为空则 addAll: 基于 status stage 全部变更)。
///
/// 事务语义: 持真 index.lock (gix 官方锁, 有界退避等待) 完成整批"读-改-写"——
/// 外部 git 进程 (agent 启动的 git CLI) 的暂存不会被旧快照静默覆盖; 整批原子
/// 落盘 (中途失败不留下部分暂存, 既有暂存保持不变)。批量实现下 index 只读/写
/// 各一次, 循环仅内存更新 entry——逐文件全量读写 index 在数万文件时是 O(n²)
/// IO, 单次 commit 可达数十分钟 (生产 43k 文件事故)。
pub fn stage_files(repo: &Repository, files: &[String]) -> AppResult<()> {
    let paths = resolve_stage_paths(repo, files)?;
    if paths.is_empty() {
        return Ok(());
    }
    let lock = acquire_index_lock(repo)?;
    let mut index = repo
        .open_index()
        .map_err(|e| map_git_err(e, "git open_index"))?;
    stage_paths_with_probe(repo, &mut index, &paths, |path| {
        std::fs::symlink_metadata(path)
    })?;
    lock.commit_index(&mut index)
}

/// stage 路径收集: 显式列表 → 展开; 空 → addAll (status 变更)。
fn resolve_stage_paths(repo: &Repository, files: &[String]) -> AppResult<Vec<String>> {
    if files.is_empty() {
        return collect_status_paths(repo);
    }
    let mut paths = Vec::with_capacity(files.len());
    for f in files {
        expand_stage_paths(repo, f, &mut paths)?;
    }
    Ok(paths)
}

/// addAll 路径收集: modified/untracked + 磁盘上已消失的 deleted。
fn collect_status_paths(repo: &Repository) -> AppResult<Vec<String>> {
    let st = get_status(repo)?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| AppError::system("git repo has no workdir"))?;
    let mut paths: Vec<String> = st
        .modified
        .iter()
        .chain(st.untracked.iter())
        .cloned()
        .collect();
    paths.extend(
        st.deleted
            .iter()
            .filter(|f| !workdir.join(f).exists())
            .cloned(),
    );
    Ok(paths)
}

/// 内存中整批更新 index entry (blob 写 ODB + entry 增删), 不落盘。
///
/// 循环期间 index 恒为原始有序状态——既有 entry 原地改字段 (不移动),
/// 新增与删除全部缓冲延后——`entry_mut_by_path_and_stage` 的二分查找因此
/// 始终有效; 排序与删除过滤各只做一次 (避免逐 entry 全表扫描/排序)。
fn stage_paths_with_probe(
    repo: &Repository,
    index: &mut gix::index::File,
    paths: &[String],
    stat_probe: impl Fn(&Path) -> std::io::Result<std::fs::Metadata>,
) -> AppResult<()> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| AppError::system("git repo has no workdir"))?;
    let mut new_entries: Vec<(Stat, ObjectId, IndexMode, BString)> = Vec::new();
    let mut deleted: HashSet<BString> = HashSet::new();
    // 以**最终 index 路径**去重: Windows 上 `dir\file.txt` 与 `dir/file.txt`
    // 归一后同键, 字符串级去重会漏; 重复 key 两条都会查不到既有 entry,
    // 延后追加将产生重复 index 条目
    let mut seen: HashSet<BString> = HashSet::with_capacity(paths.len());
    for worktree_path in paths {
        let abs = crate::path_safety::ensure_within(workdir, worktree_path)?;
        let bstr_owned = index_bstr_path(worktree_path)?;
        if !seen.insert(bstr_owned.clone()) {
            continue;
        }
        let bstr_path: &BStr = bstr_owned.as_ref();
        // NotFound = 文件确已删除 → 从 index 移除（对齐 git add 删档语义）；
        // 其他 IO 错误（EACCES/ESTALE——CephFS 瞬时 stat 失败是现实场景）必须传播：
        // 静默按删除写入 index 会让存在的文件被后续 commit 从历史里删掉。
        let metadata = match stat_probe(&abs) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(AppError::system(format!(
                    "git stat {}: {error}",
                    abs.display()
                )));
            }
        };
        let Some(metadata) = metadata else {
            deleted.insert(bstr_owned);
            continue;
        };
        let (blob_id, mode) = if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&abs)?;
            let data = into_bstr(target)
                .map_err(|error| map_git_err(error, "git encode symlink target"))?
                .into_owned();
            let id = repo
                .write_blob(data.as_slice())
                .map_err(|e| map_git_err(e, "git write symlink blob"))?
                .detach();
            (id, IndexMode::SYMLINK)
        } else if metadata.is_file() {
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            let mode = if executable {
                IndexMode::FILE_EXECUTABLE
            } else {
                IndexMode::FILE
            };
            // Repository::write_blob_stream() 在 gix 0.85 中仍先聚合到内存；
            // 直接写公开 ODB handle 才会流式压缩并落到 loose object。
            let mut file = std::fs::File::open(&abs)?;
            let id = repo
                .objects
                .write_stream(gix::objs::Kind::Blob, metadata.len(), &mut file)
                .map_err(|e| map_git_err(e, "git stream worktree blob"))?;
            (id, mode)
        } else {
            return Err(AppError::validation(format!(
                "unsupported file type: {worktree_path}"
            )));
        };
        match index.entry_mut_by_path_and_stage(bstr_path, Stage::Unconflicted) {
            Some(entry) => {
                entry.id = blob_id;
                entry.stat = Stat::default();
                entry.mode = mode;
            }
            None => new_entries.push((Stat::default(), blob_id, mode, bstr_owned)),
        }
    }
    // 删除: 单趟批量过滤 (保序, 无需再排序)
    if !deleted.is_empty() {
        index.remove_entries(|_, p, _| deleted.contains(p));
    }
    // 新增: 缓冲合并后一次追加 + 一次排序
    if !new_entries.is_empty() {
        for (stat, id, mode, path) in new_entries {
            let bstr_path: &BStr = path.as_ref();
            index.dangerously_push_entry(stat, id, Flags::empty(), mode, bstr_path);
        }
        index.sort_entries();
    }
    index.remove_tree();
    Ok(())
}

// ── index.lock 事务 ──────────────────────────────────────────────────────────

/// 锁竞争 (外部 git CLI 大仓库 add 等活跃持有者) 的有界退避等待;
/// 超时返回明确错误, 永不抢锁。
const INDEX_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// 本事务持有的 index 锁。跨进程互斥完全依赖官方 gix 锁文件:
/// 读前取得 → 持锁完成读-改-写 → `write_to` + `commit()` rename 落盘,
/// 正常失败 (含提交失败回滚) 由 gix 自动清理锁文件。
///
/// **残锁边界 (明确保留)**: 进程被强杀可能残留官方 `index.lock`, 普通重试
/// 无法解除; 该情况由后续受控诊断与恢复流程处理, 本处不用自定义协议自动
/// 删锁 (多轮复审确认: 身份判定/删除协议引入的竞态面多于其解决的问题)。
struct IndexLock {
    inner: gix::lock::File,
}

impl IndexLock {
    /// 写出 index 并 rename 提交; 失败由 gix 回滚锁文件, 原 index 保留。
    fn commit_index(self, index: &mut gix::index::File) -> AppResult<()> {
        let mut inner = self.inner;
        {
            use std::io::Write as _;
            let mut writer = std::io::BufWriter::with_capacity(64 * 1024, &mut inner);
            if let Err(error) = index.write_to(&mut writer, IndexWriteOptions::default()) {
                return Err(map_git_err(error, "git index write"));
            }
            if let Err(error) = writer.flush() {
                return Err(map_git_err(error, "git index flush"));
            }
        }
        inner
            .commit()
            .map(|_| ())
            .map_err(|e| map_git_err(e, "git index commit"))
    }
}

fn acquire_index_lock(repo: &Repository) -> AppResult<IndexLock> {
    let inner = gix::lock::File::acquire_to_update_resource(
        repo.index_path(),
        gix::lock::acquire::Fail::AfterDurationWithBackoff(INDEX_LOCK_WAIT),
        None,
        // 与 gix-index 自身 write() 及 ensure_index_file 一致: umask 前 0o666 策略
        0,
    )
    .map_err(|error| {
        AppError::system(format!(
            "git acquire index.lock: {error} \
             (index.lock is held by another git operation, or was left by a killed \
             process; retry after the operation completes, or remove .git/index.lock \
             if no git process is running)"
        ))
    })?;
    Ok(IndexLock { inner })
}

/// git/gix 同款锁文件命名: `<index>.lock`。
#[cfg(test)]
fn index_lock_path(index_path: &Path) -> std::path::PathBuf {
    let mut lock = index_path.to_path_buf();
    lock.set_file_name("index.lock");
    lock
}

/// 未解决冲突条目存在时拒绝提交 (codex v4 P1, 既有缺陷):
/// 旧实现静默跳过非 Unconflicted 条目——merge 冲突仓库提交后冲突文件会
/// 从提交树中消失 (等效提交删除)。必须在任何发布副作用之前响亮失败。
fn assert_no_unresolved_conflicts(index: &gix::index::File) -> AppResult<()> {
    let backing = index.path_backing();
    for entry in index.entries() {
        if entry.stage() != Stage::Unconflicted {
            use gix::bstr::ByteSlice as _;
            let path = entry.path_in(backing).to_str_lossy();
            return Err(AppError::business(format!(
                "unresolved merge conflict present for {path}; \
                 resolve conflicts before committing"
            )));
        }
    }
    Ok(())
}

/// 由 index 全量构造 tree (调用方须先通过 [`assert_no_unresolved_conflicts`])。
fn tree_id_from_index(repo: &Repository, index: &gix::index::File) -> AppResult<ObjectId> {
    let mut editor = repo
        .edit_tree(repo.empty_tree().id())
        .map_err(|e| map_git_err(e, "git edit_tree"))?;
    let path_backing = index.path_backing();
    for entry in index.entries() {
        let path = entry.path_in(path_backing);
        let kind = match entry.mode {
            IndexMode::FILE => TreeEntryKind::Blob,
            IndexMode::FILE_EXECUTABLE => TreeEntryKind::BlobExecutable,
            IndexMode::SYMLINK => TreeEntryKind::Link,
            IndexMode::COMMIT => TreeEntryKind::Commit,
            _ => continue,
        };
        editor
            .upsert(path, kind, entry.id)
            .map_err(|e| map_git_err(e, "git editor upsert"))?;
    }
    editor
        .write()
        .map(|id| id.detach())
        .map_err(|e| map_git_err(e, "git editor write"))
}

/// 提交当前 index。新 tree 完全由 index 构造，确保暂存删除不会被 HEAD tree 带回。
/// 带未解决冲突条目时拒绝 (不静默跳过)。
pub fn commit_indexed(
    repo: &Repository,
    message: &str,
    author_name: &str,
    author_email: &str,
) -> AppResult<String> {
    let index = repo
        .open_index()
        .map_err(|e| map_git_err(e, "git open_index"))?;
    assert_no_unresolved_conflicts(&index)?;
    let tree_id = tree_id_from_index(repo, &index)?;
    // unborn → root commit（合法首提交）；head() 真错误传播，不吞成"无 parent"
    let head = repo.head().map_err(|e| map_git_err(e, "git head"))?;
    let parents: Vec<ObjectId> = if head.is_unborn() {
        vec![]
    } else {
        vec![
            head.into_peeled_id()
                .map_err(|e| map_git_err(e, "git head_id"))?
                .detach(),
        ]
    };
    commit_to_head(repo, message, tree_id, &parents, author_name, author_email)
}

/// 写 commit 对象并发布到 HEAD (官方 `commit_as`, gix 0.90 语义):
/// - 有父提交 → HEAD 走**严格** `MustExistAndMatch(parent)`: 分支被删/前进 → 冲突报错;
/// - 首次提交 → `MustNotExist`;
/// - symbolic HEAD 经 deref 透写目标分支, **同步维护 HEAD 与分支两侧 reflog**;
///   detached HEAD 直接更新自身。
///
/// 目标为发布时刻的 HEAD (标准 git 行为), 不额外固定捕获时的分支名
/// (曾因命名分支调用走宽松 ExistingMustMatch 而手写引用事务, 已按 codex r6 复核回归官方)。
fn commit_to_head(
    repo: &Repository,
    message: &str,
    tree_id: ObjectId,
    parents: &[ObjectId],
    author_name: &str,
    author_email: &str,
) -> AppResult<String> {
    let sig = Signature {
        name: BString::from(author_name),
        email: BString::from(author_email),
        time: Time::now_local_or_utc(),
    };
    let mut buf_c = TimeBuf::default();
    let mut buf_a = TimeBuf::default();
    let commit_id = repo
        .commit_as(
            sig.to_ref(&mut buf_c),
            sig.to_ref(&mut buf_a),
            "HEAD",
            message,
            tree_id,
            parents.iter().copied(),
        )
        .map_err(|e| map_git_err(e, "git commit"))?;
    Ok(commit_id.to_string())
}

/// 事务化 stage + commit (codex v4 方案): 同一把 index.lock 内完成
/// 读 index → 内存批量 stage → 冲突拒绝 → 由**同一份内存 index** 构造 tree
/// 并与 HEAD tree 比对 (nothingToCommit 判定, 不再全工作区 status 扫描)
/// → index 单次原子落盘 → 按发布时 HEAD 严格 CAS 发布提交 (见 [`commit_to_head`])。
///
/// 失败语义: ref 冲突 (外部并行使引用前进) 时 index 已落盘 = 已暂存内容
/// 保留, 明确报错供重试; commit 对象未写入则无任何副作用。index 与 ref 是
/// 两个文件, 不宣称跨两者原子。
pub fn stage_and_commit(
    repo: &Repository,
    files: &[String],
    message: &str,
    author_name: &str,
    author_email: &str,
) -> AppResult<Option<String>> {
    let paths = resolve_stage_paths(repo, files)?;
    let lock = acquire_index_lock(repo)?;
    let mut index = repo
        .open_index()
        .map_err(|e| map_git_err(e, "git open_index"))?;
    if !paths.is_empty() {
        stage_paths_with_probe(repo, &mut index, &paths, |path| {
            std::fs::symlink_metadata(path)
        })?;
    }
    assert_no_unresolved_conflicts(&index)?;
    // 锁内捕获 HEAD 状态: parent (发布 CAS 依据); 分支由发布时的 HEAD 透写解析
    let head = repo.head().map_err(|e| map_git_err(e, "git head"))?;
    let unborn = head.is_unborn();
    let parents: Vec<ObjectId> = if unborn {
        vec![]
    } else {
        vec![
            head.into_peeled_id()
                .map_err(|e| map_git_err(e, "git head_id"))?
                .detach(),
        ]
    };
    let head_tree: Option<ObjectId> = parents
        .first()
        .map(|parent| {
            repo.find_commit(parent.to_owned())
                .map_err(|e| map_git_err(e, "git find_commit"))?
                .tree_id()
                .map(|id| id.detach())
                .map_err(|e| map_git_err(e, "git tree_id"))
        })
        .transpose()?;
    let tree_id = tree_id_from_index(repo, &index)?;
    if Some(tree_id) == head_tree {
        // nothingToCommit: 新 tree 与 HEAD 一致只说明**无需创建 commit**;
        // 本批可能修改了暂存区 (如用户把工作树恢复成 HEAD 内容, 抵消旧的
        // 暂存差异) —— 必须落盘, 否则磁盘残留旧暂存, 下次提交其他文件会
        // 连带提交 (codex 终审 P1)。锁内磁盘 index 未变: 重读比对 tree,
        // 不同才写, 真无变化 (纯 stat 刷新) 免写。
        let disk_index = repo
            .open_index()
            .map_err(|e| map_git_err(e, "git open_index"))?;
        let disk_tree = tree_id_from_index(repo, &disk_index)?;
        if disk_tree != tree_id {
            lock.commit_index(&mut index)?;
        }
        return Ok(None);
    }
    // index 先原子落盘 (失败保留原 index)
    lock.commit_index(&mut index)?;
    // 再发布提交: 官方 commit_as("HEAD") —— 严格 CAS (分支被删/前进 → 冲突),
    // deref 透写分支并同步维护两侧 reflog
    let commit_id = commit_to_head(repo, message, tree_id, &parents, author_name, author_email)?;
    Ok(Some(commit_id))
}

// ── init / unstage / discard ───────────────────────────────────────────────────

/// 初始化仓库 (对齐 nuwax init): 已存在返回 already=true; 否则 init + .gitignore + initial commit。
///
/// 幂等门是"已有提交"而非"仓库已存在": 此前 initial commit 失败 (仓库停留在
/// unborn) 时重试会真正补提交, 失败不再被吞 (fail-fast, 落盘工作区必须带提交)。
pub fn init_repo(path: &Path, author_name: &str, author_email: &str) -> AppResult<bool> {
    let existed = super::is_git_repo(path);
    let repo = super::ensure_repo(path)?;
    super::ensure_gitignore(path)?;
    if !has_any_commit(&repo)? {
        stage_path(&repo, ".gitignore")?;
        commit_indexed(&repo, "Initial commit", author_name, author_email)?;
    }
    Ok(existed)
}

/// 仓库是否已有至少一个提交（unborn → false）。
/// "缺席"只来自类型化状态（`is_unborn`），不来自错误发生——`head()` 的
/// IO/损坏错误必须传播，否则会被误判"无提交"而触发 initial commit 误写。
fn has_any_commit(repo: &Repository) -> AppResult<bool> {
    let head = repo.head().map_err(|e| map_git_err(e, "git head"))?;
    Ok(!head.is_unborn())
}

/// 公开 `/api/git/init` 使用：只初始化仓库并维护 .gitignore，不创建提交。
pub fn init_repo_only(path: &Path) -> AppResult<bool> {
    let already = super::is_git_repo(path);
    let _repo = super::ensure_repo(path)?;
    super::ensure_gitignore(path)?;
    Ok(already)
}

/// 初始化 (幂等) + stage 全部变更 + 提交 (对齐 nuwax gitService.init + commit 组合,
/// 用于 createProject/copyProject/uploadProject 在 GIT_ENABLED 下首次落地工作区)。
/// 与 [`init_repo`] 区别: 总是 stage all + 用自定义 message 提交 (非 "Initial commit")。
pub fn init_and_commit(
    path: &Path,
    message: &str,
    author_name: &str,
    author_email: &str,
) -> AppResult<()> {
    let repo = super::ensure_repo(path)?;
    super::ensure_gitignore(path)?;
    // stage 全部变更 (addAll: modified + untracked + deleted)
    stage_files(&repo, &[])?;
    // 提交 (无变更也会产生提交, 与 nuwax 一致); 失败传播——nuwax initProjectTemplate
    // 的 init+commit 组合同样不 catch, 吞错是移植自创且会留下 unborn 工作区。
    commit_indexed(&repo, message, author_name, author_email)?;
    Ok(())
}

/// unstage (git restore --staged): HEAD 有路径则恢复其 entry，HEAD 无路径则从 index 移除。
pub fn unstage_files(repo: &Repository, files: &[String]) -> AppResult<()> {
    let head_tree = repo
        .head_tree_id_or_empty()
        .map_err(|e| map_git_err(e, "git head_tree_id_or_empty"))?
        .detach();
    if files.is_empty() {
        let mut index = repo
            .index_from_tree(&head_tree)
            .map_err(|e| map_git_err(e, "git index_from_tree (unstage all)"))?;
        index.remove_tree();
        index
            .write(IndexWriteOptions::default())
            .map_err(|e| map_git_err(e, "git index write"))?;
        return Ok(());
    }

    let mut index = repo
        .open_index()
        .map_err(|e| map_git_err(e, "git open_index"))?;
    let head_index = repo
        .index_from_tree(&head_tree)
        .map_err(|e| map_git_err(e, "git index_from_tree (unstage)"))?;
    let head_backing = head_index.path_backing();
    let paths: Vec<BString> = files
        .iter()
        .map(|f| index_bstr_path(f))
        .collect::<AppResult<_>>()?;
    for p in &paths {
        let bp: &BStr = p.as_ref();
        index.remove_entries(|_, x, _| x == bp);
        if let Some(entry) = head_index.entry_by_path_and_stage(bp, Stage::Unconflicted) {
            index.dangerously_push_entry(
                entry.stat,
                entry.id,
                entry.flags,
                entry.mode,
                entry.path_in(head_backing),
            );
        }
    }
    index.sort_entries();
    index.remove_tree();
    index
        .write(IndexWriteOptions::default())
        .map_err(|e| map_git_err(e, "git index write"))?;
    Ok(())
}

/// discard 分桶结果 (对齐 nuwax discard 响应明细)。
#[derive(Debug, Default, Clone)]
pub struct DiscardBuckets {
    pub tracked_files: Vec<String>,
    pub new_files: Vec<String>,
    pub untracked_files: Vec<String>,
}

impl DiscardBuckets {
    pub fn len(&self) -> usize {
        self.tracked_files.len() + self.new_files.len() + self.untracked_files.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// discard (对齐 nuwax discard): files 空 → discard 全部 modified/staged/deleted。
/// - HEAD 有该文件 (tracked, modified/deleted) → 恢复 worktree 到 HEAD + `stage_path` 同步 index
/// - HEAD 无 (untracked/staged-new) → 删除 worktree + `stage_path` 同步 index (移除 staged-new entry)
///
/// 返回三桶明细 (trackedFiles / newFiles / untrackedFiles), 与 nuwax 一致。
pub fn discard_files(repo: &Repository, files: &[String]) -> AppResult<DiscardBuckets> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| AppError::system("git repo has no workdir"))?;
    let head_tree_id = repo
        .head_tree_id_or_empty()
        .map_err(|e| map_git_err(e, "git head_tree_id_or_empty"))?;
    let tree = repo
        .find_tree(head_tree_id)
        .map_err(|e| map_git_err(e, "git find_tree"))?;
    let st = get_status(repo)?;
    let staged_set: HashSet<String> = st.staged.iter().cloned().collect();
    let to_discard: Vec<String> = if files.is_empty() {
        // 空 files → discard 全部变更 (对齐 nuwax discard: tracked-modified/deleted + staged-new
        // + **untracked**; 漏 untracked 会导致 discard-all 后仍有未跟踪文件残留)
        st.modified
            .iter()
            .chain(st.deleted.iter())
            .chain(st.staged.iter())
            .chain(st.untracked.iter())
            .cloned()
            .collect()
    } else {
        files.to_vec()
    };
    let mut buckets = DiscardBuckets::default();
    for f in &to_discard {
        // 安全：用户可控的 files 必须在任何 fs 操作前做路径校验，拒绝 `..` / 绝对路径穿越。
        // stage_path 内部的 ensure_within 发生在 write/remove 之后（太晚），故在此先行拦截。
        let abs = crate::path_safety::ensure_within(workdir, f)?;
        match tree
            .lookup_entry_by_path(f)
            .map_err(|e| map_git_err(e, "git lookup_entry_by_path"))?
        {
            Some(entry) => {
                // tracked (modified/deleted): 恢复 worktree 到 HEAD 内容 + 同步 index
                // （共享安全物化器: 界内目录链 + leaf 不跟随 + mode 如实恢复, FS-03/06）
                let planned = super::materialize::plan_tree_entry(
                    workdir,
                    Path::new(f),
                    entry.mode(),
                    entry.id(),
                )?;
                super::materialize::materialize_planned(repo, workdir, &planned)?;
                stage_path(repo, f)?;
                buckets.tracked_files.push(f.clone());
            }
            None => {
                // untracked / staged-new: 删 worktree + 同步 index (移除 staged-new entry)
                if let Err(e) = std::fs::remove_file(&abs) {
                    tracing::warn!(error = %e, "remove untracked worktree file failed (skipping)");
                }
                stage_path(repo, f)?;
                if staged_set.contains(f) {
                    buckets.new_files.push(f.clone());
                } else {
                    buckets.untracked_files.push(f.clone());
                }
            }
        }
    }
    Ok(buckets)
}

/// [`init_repo`] / [`init_and_commit`] 的 async 安全包装：git2 全同步 IO
/// （index 写、对象写、commit——在 Ceph RBD/CephFS 卷上含多次 fsync，高负载
/// 下可达秒级）。此前在 async 上下文直接调用会阻塞 runtime worker：accept
/// 停滞 → readiness/liveness 探针与业务转发的连接被 reset（131 userapp 套件
/// build 转发 IncompleteMessage 的根因链；同因诱发 liveness 30s 窗口误杀）。
/// 挪 blocking 池执行；git 失败语义不变（调用方 best-effort warn）。
pub async fn init_repo_offloaded(
    path: std::path::PathBuf,
    author_name: String,
    author_email: String,
) -> AppResult<bool> {
    tokio::task::spawn_blocking(move || {
        // init 的 stage/commit 与 /api/git/* 写操作共用工作区锁 (codex v2 P2)
        let _guard = super::workspace_lock::acquire(&path)?;
        init_repo(&path, &author_name, &author_email)
    })
    .await
    .map_err(|e| map_git_err(e, "git init_repo join"))?
}

/// [`init_and_commit`] 的 async 安全包装（同 [`init_repo_offloaded`] 动因）。
pub async fn init_and_commit_offloaded(
    path: std::path::PathBuf,
    message: String,
    author_name: String,
    author_email: String,
) -> AppResult<()> {
    tokio::task::spawn_blocking(move || {
        let _guard = super::workspace_lock::acquire(&path)?;
        init_and_commit(&path, &message, &author_name, &author_email)
    })
    .await
    .map_err(|e| map_git_err(e, "git init_and_commit join"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::open;

    #[test]
    fn init_repo_propagates_corrupt_head_without_touching_index() {
        // G-C1 反例: "缺席"判定只认 is_unborn 状态。HEAD 损坏 = 真错误, 必须
        // 响亮失败, 不得误判"无提交"后 stage .gitignore 甚至写出孤立 root commit。
        let dir = tempfile::tempdir().expect("create test directory");
        let path = dir.path();
        super::super::ensure_repo(path).expect("init unborn repo");
        std::fs::write(path.join(".git/HEAD"), "garbage-not-a-ref\n").expect("corrupt HEAD");

        let err = init_repo(path, "Test", "test@example.com").expect_err("损坏 HEAD 必须响亮失败");
        assert!(matches!(err, AppError::System(..)), "{err:?}");

        let repo = open(path).expect("open repo");
        let index = repo.open_index().expect("open index");
        assert!(
            index.entries().is_empty(),
            "失败的 init_repo 不得改动 index: {} entries",
            index.entries().len()
        );
    }

    /// 反例（审查 H-1）：stat 的非 NotFound IO 错误（EACCES/ESTALE——CephFS
    /// 现实场景）曾被 `.ok()` 当作"文件已删除"写入 index 并持久化，存在的
    /// 文件会被后续 commit 从历史里删掉。修复后必须传播错误、index 不动。
    #[test]
    fn stage_file_propagates_stat_io_error_instead_of_treating_as_deleted() {
        let directory = tempfile::tempdir().expect("test repository");
        let dir = directory.path();
        std::fs::create_dir_all(dir.join("protected")).expect("建目录");
        init_repo(dir, "Test", "test@example.com").expect("init");
        std::fs::write(dir.join("protected/a.txt"), "alive").expect("写文件");
        let repo = open(dir).expect("open");
        stage_path(&repo, "protected/a.txt").expect("正常 stage");
        commit_indexed(&repo, "c1", "Test", "test@example.com").expect("commit");

        let before = std::fs::read(repo.index_path()).expect("index bytes");
        // 受控 stat 故障走完整 stage 分支，不依赖 chmod 对 root/Windows 的行为。
        // 直接注入到内存批量 stage: 事务语义下错误即中止, 不落盘。
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::Other,
        ] {
            let mut index = repo.open_index().expect("open index");
            let error = stage_paths_with_probe(
                &repo,
                &mut index,
                &["protected/a.txt".to_string()],
                |path| {
                    assert_eq!(path, dir.join("protected/a.txt"));
                    Err(std::io::Error::new(kind, "injected stat failure"))
                },
            )
            .expect_err("stat IO 错误必须传播而非按删除处理");
            assert!(
                matches!(error, AppError::System(_)),
                "应为系统错误: {error:?}"
            );
            drop(index);
            assert_eq!(
                std::fs::read(repo.index_path()).expect("unchanged index"),
                before
            );
        }
        // index 未被写入删除态：文件仍在 index 中。
        let index = repo.open_index().expect("reopen index");
        assert!(
            index
                .entry_by_path_and_stage(
                    gix::bstr::ByteSlice::as_bstr(b"protected/a.txt"),
                    Stage::Unconflicted
                )
                .is_some(),
            "存在的文件不得因 stat 失败被移出 index"
        );
    }

    /// 批量 stage 中途失败必须原子: 既有暂存保留, 本批任何文件都不落盘
    /// (codex 审查要求的失败回归)。
    #[test]
    fn batch_stage_failure_preserves_prior_staging_and_stages_nothing() {
        let test = TestRepo::new();
        commit_file(&test, "base.txt", "base", "add base");
        std::fs::write(test.0.join("staged-earlier.txt"), "earlier").expect("write earlier");
        let repo = test.open();
        // 先单独 stage 一个文件 (模拟用户此前已暂存的内容)
        stage_path(&repo, "staged-earlier.txt").expect("stage earlier");
        let before = std::fs::read(repo.index_path()).expect("index bytes");

        // 批量 [新文件, stat 注入失败]: 整批失败
        std::fs::write(test.0.join("new-in-batch.txt"), "new").expect("write new");
        let files = vec![
            "new-in-batch.txt".to_string(),
            "staged-earlier.txt".to_string(),
        ];
        let error = {
            let mut index = repo.open_index().expect("open index");
            let result = stage_paths_with_probe(&repo, &mut index, &files, |path| {
                if path.ends_with("new-in-batch.txt") {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected",
                    ))
                } else {
                    std::fs::symlink_metadata(path)
                }
            });
            match result {
                Ok(()) => panic!("batch with injected stat failure must fail"),
                Err(error) => error,
            }
        };
        assert!(matches!(error, AppError::System(_)));

        // index 逐字节不变: 既有暂存保留, 批内新文件未写入
        assert_eq!(
            std::fs::read(repo.index_path()).expect("index after failure"),
            before
        );
        let st = get_status(&repo).expect("status");
        assert!(
            st.staged.iter().any(|f| f == "staged-earlier.txt"),
            "prior staging must survive: {:?}",
            st.staged
        );
        assert!(
            !st.staged.iter().any(|f| f == "new-in-batch.txt"),
            "failed batch must not stage anything"
        );
    }

    /// 外部进程持有的新鲜 index.lock: stage 必须明确失败, 不覆盖不抢锁
    /// (codex 复现的 a+b → 仅 a 静默丢失场景, 修复后应报冲突)。
    /// 锁等待有退避上限, 该测试会占用 INDEX_LOCK_WAIT (10s) 一次。
    #[test]
    fn fresh_external_index_lock_is_refused_not_overwritten() {
        let test = TestRepo::new();
        commit_file(&test, "a.txt", "a", "add a");
        std::fs::write(test.0.join("b.txt"), "b").expect("write b");
        let repo = test.open();

        // 模拟外部 git CLI 持锁: 手工创建新鲜 index.lock
        let lock_path = index_lock_path(&repo.index_path());
        std::fs::write(&lock_path, b"partial").expect("create lock");

        let error = stage_files(&repo, &["b.txt".to_string()])
            .expect_err("fresh external lock must be refused");
        assert!(
            format!("{error}").contains("index.lock"),
            "error should mention index.lock: {error}"
        );
        // 锁未被抢走, index 未被写: 外部暂存语义不受影响
        assert!(lock_path.exists(), "live lock must not be stolen");
        // 清理: 释放后 stage 可正常完成
        std::fs::remove_file(&lock_path).expect("remove lock");
        stage_files(&repo, &["b.txt".to_string()]).expect("stage after lock release");
        let st = get_status(&repo).expect("status");
        assert!(st.staged.iter().any(|f| f == "b.txt"));
    }

    /// 未解决冲突条目必须拒绝提交 (codex v4 P1 既有缺陷回归):
    /// 旧实现静默跳过非 Unconflicted 条目, 冲突文件从提交树消失。
    #[test]
    fn commit_rejects_unresolved_conflict_entries() {
        let test = TestRepo::new();
        commit_file(&test, "f.txt", "base", "base");
        let repo = test.open();

        // 构造冲突 index: 同路径追加 stage 位非 0 的 entry ( Theirs )
        use gix::bstr::ByteSlice as _;
        let mut index = repo.open_index().expect("open index");
        let blob = repo.write_blob(b"theirs").expect("blob").detach();
        index.dangerously_push_entry(
            Stat::default(),
            blob,
            Flags::from_bits_retain(Flags::STAGE_MASK.bits()),
            IndexMode::FILE,
            b"f.txt".as_bstr(),
        );
        index.sort_entries();
        let mut lock = gix::lock::File::acquire_to_update_resource(
            repo.index_path(),
            gix::lock::acquire::Fail::Immediately,
            None,
            0,
        )
        .expect("lock for fixture write");
        {
            use std::io::Write as _;
            let mut writer = std::io::BufWriter::new(&mut lock);
            index
                .write_to(&mut writer, IndexWriteOptions::default())
                .expect("write conflicted index");
            writer.flush().expect("flush");
        }
        lock.commit().expect("commit fixture index");

        let error = commit_indexed(&repo, "on conflict", "Test", "test@example.com")
            .expect_err("conflicted index must be rejected");
        assert!(
            format!("{error}").contains("unresolved merge conflict"),
            "error should explain the conflict: {error}"
        );
        // HEAD 未动
        assert!(
            repo.head_commit().is_ok(),
            "HEAD must remain valid after rejection"
        );
    }

    /// 事务化 stage+commit: 显式列表提交 / nothingToCommit / 冲突传播。
    #[test]
    fn stage_and_commit_commits_and_detects_nothing_to_commit() {
        let test = TestRepo::new();
        commit_file(&test, "a.txt", "one", "add a");
        let repo = test.open();

        // 修改后提交 → Some(hash), 只含该变更
        std::fs::write(test.0.join("a.txt"), "two").expect("modify a");
        let hash = stage_and_commit(&repo, &["a.txt".to_string()], "m1", "T", "t@e.c")
            .expect("commit")
            .expect("must produce a commit");
        assert!(!hash.is_empty());
        assert_eq!(
            std::fs::read_to_string(test.0.join("a.txt")).expect("worktree intact"),
            "two"
        );

        // 内容未变重提交 → None (nothingToCommit, 不写 index 不发布)
        let again = stage_and_commit(&repo, &["a.txt".to_string()], "m2", "T", "t@e.c")
            .expect("no-error on nothing to commit");
        assert!(again.is_none(), "unchanged content must be nothingToCommit");
        assert_eq!(
            repo.head_commit().expect("head").id().to_string(),
            hash,
            "nothingToCommit must not move HEAD"
        );

        // 新文件 addAll (空列表) → Some
        std::fs::write(test.0.join("b.txt"), "b").expect("add b");
        let hash2 = stage_and_commit(&repo, &[], "m3", "T", "t@e.c")
            .expect("addAll commit")
            .expect("commit hash");
        assert_ne!(hash, hash2);
    }

    /// nothingToCommit 也必须落盘本批对暂存区的有效修改 (codex 终审 P1):
    /// 旧暂存内容与 HEAD 有差异, 用户把工作树恢复成 HEAD 内容后提交——
    /// 结果应是 None (无新 commit) 且旧暂存从磁盘 index 消失。
    #[test]
    fn nothing_to_commit_persists_staging_restoration() {
        let test = TestRepo::new();
        commit_file(&test, "a.txt", "original", "add a");
        let repo = test.open();

        // 暂存另一版本
        std::fs::write(test.0.join("a.txt"), "old-staged-content").expect("stage v2");
        stage_files(&repo, &["a.txt".to_string()]).expect("stage");
        // 工作树恢复为 HEAD 内容
        std::fs::write(test.0.join("a.txt"), "original").expect("restore");

        let result = stage_and_commit(&repo, &["a.txt".to_string()], "m", "T", "t@e.c")
            .expect("commit must not fail");
        assert!(result.is_none(), "tree==HEAD → nothingToCommit");

        // 旧暂存必须已从磁盘 index 消失 (重新打开仓库验证, 排除内存态)
        let reopened = open(&test.0).expect("reopen repo");
        let st = get_status(&reopened).expect("status after");
        assert!(
            st.staged.is_empty(),
            "stale staging must be gone from disk index: {:?}",
            st.staged
        );
        // 再次提交同文件 → 仍 None, HEAD 不动
        assert!(
            stage_and_commit(&reopened, &["a.txt".to_string()], "m2", "T", "t@e.c")
                .expect("retry")
                .is_none()
        );
    }

    /// 多文件全部恢复: 显式列表只控制暂存范围, 提交的是整个 index——
    /// 当全部暂存差异被恢复抵消时 nothingToCommit, 磁盘暂存区同步清空。
    #[test]
    fn nothing_to_commit_persists_multi_file_restoration() {
        let test = TestRepo::new();
        commit_file(&test, "a.txt", "original-a", "add a");
        commit_file(&test, "b.txt", "original-b", "add b");
        let repo = test.open();

        std::fs::write(test.0.join("a.txt"), "staged-a").expect("stage a v2");
        std::fs::write(test.0.join("b.txt"), "staged-b").expect("stage b v2");
        stage_files(&repo, &["a.txt".to_string(), "b.txt".to_string()]).expect("stage");
        // 两文件都恢复成 HEAD 内容后提交 (addAll 空列表路径)
        std::fs::write(test.0.join("a.txt"), "original-a").expect("restore a");
        std::fs::write(test.0.join("b.txt"), "original-b").expect("restore b");

        let result = stage_and_commit(&repo, &[], "m", "T", "t@e.c").expect("addAll must not fail");
        assert!(result.is_none(), "全部暂存差异被抵消 → nothingToCommit");

        let reopened = open(&test.0).expect("reopen repo");
        let st = get_status(&reopened).expect("status");
        assert!(
            st.staged.is_empty(),
            "stale staging must be gone: {:?}",
            st.staged
        );
    }

    /// 捕获后被删除的分支: 严格 CAS 必须报冲突且**不复活**分支 (codex 终审 P2)。
    #[test]
    fn publish_cas_refuses_deleted_branch_without_recreating() {
        let test = TestRepo::new();
        let first = commit_file(&test, "a.txt", "a", "add a");
        let repo = test.open();

        // 捕获 main 的 parent, 然后删除 main
        let head = repo.head().expect("head");
        let parent = head.into_peeled_id().expect("peel").detach();
        {
            use gix::refs::FullName;
            use gix::refs::transaction::{Change, PreviousValue, RefEdit, RefLog};
            repo.edit_references(std::iter::once(RefEdit {
                change: Change::Delete {
                    expected: PreviousValue::Any,
                    log: RefLog::AndReference,
                },
                name: FullName::try_from("refs/heads/main").expect("valid name"),
                deref: false,
            }))
            .expect("delete main");
        }

        // 按捕获的 parent 发布 → 官方 HEAD 严格 CAS 必须冲突
        let index = repo.open_index().expect("index");
        let new_tree = tree_id_from_index(&repo, &index).expect("tree");
        let error = commit_to_head(
            &repo,
            "orphan",
            new_tree,
            std::slice::from_ref(&parent),
            "T",
            "t@e.c",
        )
        .expect_err("deleted branch must conflict");

        // 分支不得被复活
        assert!(
            repo.find_reference("refs/heads/main").is_err(),
            "deleted branch must not be recreated: {error}"
        );
        drop(first);
    }

    /// CAS 冲突后的可恢复性: 发布冲突不留下任何锁定/损坏状态,
    /// 下一次 stage_and_commit 立即按最新 HEAD 正常提交。
    #[test]
    fn repo_stays_usable_after_publish_conflict() {
        let test = TestRepo::new();
        commit_file(&test, "a.txt", "a", "add a");
        let repo = test.open();

        // 用过期的 parent 模拟"捕获后引用被外部前进"的发布冲突
        let stale_parent = repo
            .head()
            .expect("head")
            .into_peeled_id()
            .expect("peel")
            .detach();
        // 外部先前进 main (模拟并发提交)
        std::fs::write(test.0.join("b.txt"), "external").expect("external file");
        stage_files(&repo, &[]).expect("external stage");
        commit_indexed(&repo, "external commit", "T", "t@e.c").expect("external commit lands");
        let advanced = repo
            .head()
            .expect("head")
            .into_peeled_id()
            .expect("peel")
            .detach();
        assert_ne!(stale_parent, advanced, "前置: 外部已前进 main");

        // 拿旧 parent 发布 → 官方 HEAD 严格 CAS 必须冲突
        let index = repo.open_index().expect("index");
        let tree = tree_id_from_index(&repo, &index).expect("tree");
        let conflict = commit_to_head(
            &repo,
            "stale attempt",
            tree,
            std::slice::from_ref(&stale_parent),
            "T",
            "t@e.c",
        )
        .expect_err("stale parent must conflict");
        assert!(
            format!("{conflict}").contains("reference"),
            "error should mention the reference conflict: {conflict}"
        );
        // main 仍指向外部提交, 未被覆盖/未复活
        let current = repo
            .head()
            .expect("head after conflict")
            .into_peeled_id()
            .expect("peel")
            .detach();
        assert_eq!(
            current, advanced,
            "external commit must survive the conflict"
        );

        // 冲突后正常提交立即成功 (重新捕获最新 HEAD)
        std::fs::write(test.0.join("a.txt"), "retry content").expect("modify");
        let hash = stage_and_commit(
            &repo,
            &["a.txt".to_string()],
            "after conflict",
            "T",
            "t@e.c",
        )
        .expect("repo must remain usable after publish conflict")
        .expect("commit hash");
        let parent_of_new = repo
            .find_commit(ObjectId::from_hex(hash.as_bytes()).expect("parse hash"))
            .expect("find new commit")
            .parent_ids()
            .next()
            .map(|id| id.detach());
        assert_eq!(
            parent_of_new,
            Some(advanced),
            "retry must land on the advanced (external) head, not the stale one"
        );
    }

    /// 分支提交必须同步更新 HEAD reflog (codex r6 P2):
    /// 此前直写分支漏 HEAD 日志, `git reflog` 停在旧提交, 影响 `HEAD@{n}` 定位。
    /// 覆盖: 普通分支提交 / unborn 首次提交 / detached HEAD。
    #[test]
    fn commit_updates_head_reflog() {
        let test = TestRepo::new();
        // unborn 首次提交: HEAD reflog 应记录 root commit
        let root = commit_file(&test, "a.txt", "a", "root commit");
        assert_head_reflog_ends_with(&test.0, &root, "unborn root commit");

        // 普通分支提交
        let repo = test.open();
        std::fs::write(test.0.join("a.txt"), "v2").expect("modify");
        let second = stage_and_commit(&repo, &["a.txt".to_string()], "second", "T", "t@e.c")
            .expect("commit")
            .expect("hash");
        assert_head_reflog_ends_with(&test.0, &second, "branch commit");

        // detached HEAD 提交: 直接把 HEAD 写成 oid
        std::fs::write(test.0.join(".git/HEAD"), format!("{root}\n")).expect("detach HEAD");
        std::fs::write(test.0.join("a.txt"), "v3").expect("modify again");
        let detached_repo = test.open();
        let detached = stage_and_commit(
            &detached_repo,
            &["a.txt".to_string()],
            "detached",
            "T",
            "t@e.c",
        )
        .expect("commit")
        .expect("hash");
        assert_head_reflog_ends_with(&test.0, &detached, "detached commit");
    }

    fn assert_head_reflog_ends_with(workdir: &Path, expected_hex: &str, case: &str) {
        let reflog = std::fs::read_to_string(workdir.join(".git/logs/HEAD"))
            .unwrap_or_else(|_| panic!("{case}: HEAD reflog missing"));
        let last_new_oid = reflog
            .lines()
            .last()
            .unwrap_or_else(|| panic!("{case}: HEAD reflog empty"))
            .split_whitespace()
            .nth(1)
            .unwrap_or_else(|| panic!("{case}: malformed reflog line"));
        assert_eq!(
            last_new_oid, expected_hex,
            "{case}: HEAD reflog must end with the new commit"
        );
    }

    struct TestRepo(std::path::PathBuf);

    impl TestRepo {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "file-server-git-test-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create test repo");
            init_repo(&path, "Test", "test@example.com").expect("init test repo");
            Self(path)
        }

        fn open(&self) -> Repository {
            open(&self.0).expect("open test repo")
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.0));
        }
    }

    fn commit_file(test: &TestRepo, path: &str, data: &str, message: &str) -> String {
        std::fs::write(test.0.join(path), data).expect("write fixture");
        let repo = test.open();
        stage_path(&repo, path).expect("stage fixture");
        commit_indexed(&repo, message, "Test", "test@example.com").expect("commit fixture")
    }

    #[test]
    fn stage_files_batches_mixed_ops_into_single_commit() {
        let test = TestRepo::new();
        commit_file(&test, "a.txt", "one", "add a");
        commit_file(&test, "b.txt", "two", "add b");
        commit_file(&test, "c.txt", "three", "add c");
        // 修改 a + 删除 b + 新增 d, 路径列表含重复项 (幂等)
        std::fs::write(test.0.join("a.txt"), "one-mod").expect("modify a");
        std::fs::remove_file(test.0.join("b.txt")).expect("remove b");
        std::fs::write(test.0.join("d.txt"), "new").expect("add d");
        let repo = test.open();
        let files = vec![
            "a.txt".to_string(),
            "b.txt".to_string(),
            "d.txt".to_string(),
            "a.txt".to_string(),
        ];
        stage_files(&repo, &files).expect("batch stage");
        let st = get_status(&repo).expect("status");
        let mut staged = st.staged.clone();
        staged.sort();
        assert_eq!(staged, vec!["a.txt", "b.txt", "d.txt"]);
        commit_indexed(&repo, "batch", "Test", "test@example.com").expect("commit");
        let tree = repo.head_tree().expect("head tree");
        assert!(
            tree.lookup_entry_by_path("b.txt")
                .expect("lookup")
                .is_none()
        );
        assert!(
            tree.lookup_entry_by_path("d.txt")
                .expect("lookup")
                .is_some()
        );
    }

    #[test]
    fn stage_files_expands_directory_paths() {
        let test = TestRepo::new();
        std::fs::create_dir_all(test.0.join("dir/nested")).expect("create dirs");
        commit_file(&test, "dir/outer.txt", "outer", "add outer");
        commit_file(&test, "dir/nested/inner.txt", "inner", "add inner");
        std::fs::write(test.0.join("dir/nested/inner.txt"), "inner-mod").expect("modify nested");
        std::fs::write(test.0.join("dir/new.txt"), "new").expect("add new in dir");
        let repo = test.open();
        stage_files(&repo, &["dir".to_string()]).expect("stage dir");
        let st = get_status(&repo).expect("status");
        let mut staged = st.staged.clone();
        staged.sort();
        assert_eq!(staged, vec!["dir/nested/inner.txt", "dir/new.txt"]);
    }

    #[test]
    fn stage_files_empty_list_stages_all_changes() {
        let test = TestRepo::new();
        commit_file(&test, "base.txt", "base", "base");
        std::fs::write(test.0.join("base.txt"), "mod").expect("modify");
        std::fs::write(test.0.join("untracked.txt"), "new").expect("untracked");
        std::fs::remove_file(test.0.join("base.txt")).expect("replace by delete below");
        std::fs::write(test.0.join("extra.txt"), "extra").expect("extra untracked");
        let repo = test.open();
        stage_files(&repo, &[]).expect("stage all");
        let st = get_status(&repo).expect("status");
        let mut staged = st.staged.clone();
        staged.sort();
        // base.txt 的删除也进入 staged (git add -A 语义)
        assert_eq!(staged, vec!["base.txt", "extra.txt", "untracked.txt"]);
    }

    #[test]
    fn stage_files_unchanged_paths_leave_staged_empty() {
        let test = TestRepo::new();
        commit_file(&test, "same.txt", "same", "add");
        let repo = test.open();
        stage_files(&repo, &["same.txt".to_string()]).expect("stage unchanged");
        let st = get_status(&repo).expect("status");
        assert!(st.staged.is_empty(), "重 stage 未变更文件必须产生空 staged");
    }

    #[test]
    fn commit_tree_honours_staged_deletion() {
        let test = TestRepo::new();
        commit_file(&test, "delete-me.txt", "one", "add file");
        std::fs::remove_file(test.0.join("delete-me.txt")).expect("remove fixture");
        let repo = test.open();
        stage_path(&repo, "delete-me.txt").expect("stage deletion");
        commit_indexed(&repo, "delete file", "Test", "test@example.com").expect("commit deletion");
        let tree = repo.head_tree().expect("head tree");
        assert!(
            tree.lookup_entry_by_path("delete-me.txt")
                .expect("tree lookup")
                .is_none(),
            "deleted index entry must not be copied back from HEAD"
        );
    }

    #[test]
    fn discard_restores_tracked_file_from_head() {
        let test = TestRepo::new();
        commit_file(&test, "tracked.txt", "head", "add tracked");
        std::fs::write(test.0.join("tracked.txt"), "modified").expect("modify fixture");
        let repo = test.open();
        let buckets = discard_files(&repo, &["tracked.txt".to_string()]).expect("discard tracked");
        assert_eq!(buckets.tracked_files, vec!["tracked.txt".to_string()]);
        // worktree 恢复到 HEAD 内容
        assert_eq!(
            std::fs::read_to_string(test.0.join("tracked.txt")).unwrap(),
            "head"
        );
    }

    #[test]
    fn discard_rejects_path_traversal() {
        let test = TestRepo::new();
        commit_file(&test, "tracked.txt", "head", "add tracked");
        let repo = test.open();
        // 穿越路径必须在任何 fs 操作前被 ensure_within 拦截
        let outside = test.0.parent().unwrap().join("evil-outside.txt");
        drop(std::fs::remove_file(&outside));
        let result = discard_files(&repo, &["../evil-outside.txt".to_string()]);
        assert!(
            result.is_err(),
            "path traversal must be rejected before any fs op"
        );
        assert!(!outside.exists(), "no fs side effect outside workdir");
    }

    #[test]
    fn unstage_restores_tracked_entry_from_head() {
        let test = TestRepo::new();
        commit_file(&test, "tracked.txt", "head", "add tracked");
        std::fs::write(test.0.join("tracked.txt"), "worktree").expect("modify fixture");
        let repo = test.open();
        stage_path(&repo, "tracked.txt").expect("stage modification");
        unstage_files(&repo, &["tracked.txt".to_string()]).expect("unstage modification");

        let index = repo.open_index().expect("open index");
        let index_entry = index
            .entry_by_path_and_stage("tracked.txt".into(), Stage::Unconflicted)
            .expect("tracked index entry");
        let head_entry = repo
            .head_tree()
            .expect("head tree")
            .lookup_entry_by_path("tracked.txt")
            .expect("head lookup")
            .expect("tracked head entry");
        assert_eq!(index_entry.id, head_entry.id().detach());
        assert_eq!(
            std::fs::read_to_string(test.0.join("tracked.txt")).expect("read worktree"),
            "worktree",
            "unstage must not modify the worktree"
        );
    }

    #[test]
    fn checkout_overlay_preserves_non_target_index_entries() {
        let test = TestRepo::new();
        let target = commit_file(&test, "a.txt", "a1", "add a");
        commit_file(&test, "b.txt", "b1", "add b");
        let repo = test.open();
        super::super::ops::checkout_tree(&repo, &target).expect("checkout old tree");
        let index = repo.open_index().expect("open index");
        assert!(
            index
                .entry_by_path_and_stage("b.txt".into(), Stage::Unconflicted)
                .is_some(),
            "checkout overlay must not stage deletion of paths absent from target"
        );
    }

    fn commit_count(repo: &Repository) -> usize {
        crate::service::git::read::log_history(repo, 50, 0, None, None)
            .expect("log count")
            .len()
    }

    #[test]
    fn init_repo_is_idempotent_and_leaves_born_repo() {
        // 不变量: 走 provisioning 的工作区必须带着初始提交落地
        let test = TestRepo::new();
        let repo = test.open();
        assert!(
            repo.head_id().is_ok(),
            "全新目录 init_repo 后必须已有初始提交"
        );
        let before = commit_count(&repo);
        let existed = init_repo(&test.0, "Test", "test@example.com").expect("re-init");
        assert!(existed, "已存在仓库的第二次调用应返回 already=true");
        assert_eq!(
            commit_count(&repo),
            before,
            "已有提交的仓库不得重复补 Initial commit"
        );
    }

    #[test]
    fn init_repo_retries_initial_commit_on_unborn_repo() {
        // 粘性失败回归: 仓库已存在但无提交 (初始提交曾失败 / init-only) 时,
        // 修复前的 already=is_git_repo 门会跳过提交, unborn 状态永远无法自愈
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "file-server-git-test-unborn-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create unborn repo dir");
        crate::service::git::ensure_repo(&path).expect("init only (no commit)");
        let existed = init_repo(&path, "Test", "test@example.com").expect("retry init_repo");
        assert!(existed, "仓库已存在, 返回值语义保持 already=true");
        let repo = open(&path).expect("open after retry");
        assert!(
            repo.head_id().is_ok(),
            "unborn 仓库上的 init_repo 必须补出初始提交"
        );
        drop(std::fs::remove_dir_all(&path));
    }

    #[cfg(unix)]
    #[test]
    fn stage_symlink_preserves_non_utf8_target_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let directory = tempfile::tempdir().expect("repository directory");
        let repo = super::super::ensure_repo(directory.path()).expect("repository");
        let target = b"target-\xff";
        std::os::unix::fs::symlink(
            std::ffi::OsString::from_vec(target.to_vec()),
            directory.path().join("link"),
        )
        .expect("raw-byte symlink target");
        stage_path(&repo, "link").expect("lossless symlink staging");
        let index = repo.open_index().expect("index");
        let entry = index
            .entry_by_path_and_stage(BStr::new(b"link"), Stage::Unconflicted)
            .expect("staged link");
        assert_eq!(entry.mode, IndexMode::SYMLINK);
        let blob = repo.find_blob(entry.id).expect("target blob");
        assert_eq!(
            blob.data.as_slice(),
            target,
            "path conversion cannot replace bytes with Unicode replacement characters"
        );
    }
}

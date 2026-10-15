use super::receipt::{self, Action, Active, Receipt, State};
use crate::error::{AppError, AppResult};
use std::path::{Path, PathBuf};

pub(super) fn plan(
    root: &Path,
    directory: &str,
    exclude_dirs: &[String],
    exclude_files: &[String],
) -> AppResult<Vec<Action>> {
    let mut actions = Vec::new();
    let incoming = root.join(directory).join("incoming");
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == directory || name == receipt::LOCK || name == receipt::ACTIVE {
            continue;
        }
        let ty = entry.file_type()?;
        let keep = (ty.is_dir() && exclude_dirs.iter().any(|s| name == s.as_str()))
            || (ty.is_file() && exclude_files.iter().any(|s| name == s.as_str()));
        if !keep {
            save_old(root, directory, Path::new(&name), &mut actions)?;
        }
    }
    for entry in std::fs::read_dir(&incoming)? {
        let entry = entry?;
        let target = PathBuf::from(entry.file_name());
        if receipt::reserved(&target) || target == Path::new(directory) {
            return Err(AppError::validation(
                "restore archive conflicts with operation metadata",
            ));
        }
        let current = match std::fs::symlink_metadata(root.join(&target)) {
            Ok(meta) => Some(meta),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let retained = current.is_some_and(|m| {
            (m.is_dir()
                && exclude_dirs
                    .iter()
                    .any(|s| target.as_os_str() == s.as_str()))
                || (m.is_file()
                    && exclude_files
                        .iter()
                        .any(|s| target.as_os_str() == s.as_str()))
        });
        let source = Path::new(directory).join("incoming").join(&target);
        if retained {
            merge_retained(root, directory, &source, &target, &mut actions)?;
        } else {
            install(root, &source, &target, &mut actions)?;
        }
    }
    Ok(actions)
}

fn save_old(
    root: &Path,
    directory: &str,
    target: &Path,
    actions: &mut Vec<Action>,
) -> AppResult<()> {
    let id = receipt::identity(&root.join(target))?
        .ok_or_else(|| AppError::system("restore source disappeared while planning"))?;
    actions.push(Action {
        source: target.to_owned(),
        target: Path::new(directory)
            .join("backup")
            .join(actions.len().to_string()),
        identity: id,
    });
    Ok(())
}
fn install(root: &Path, source: &Path, target: &Path, actions: &mut Vec<Action>) -> AppResult<()> {
    let id = receipt::identity(&root.join(source))?
        .ok_or_else(|| AppError::system("staged restore entry disappeared while planning"))?;
    actions.push(Action {
        source: source.to_owned(),
        target: target.to_owned(),
        identity: id,
    });
    Ok(())
}
fn merge_retained(
    root: &Path,
    directory: &str,
    source: &Path,
    target: &Path,
    actions: &mut Vec<Action>,
) -> AppResult<()> {
    let incoming = std::fs::symlink_metadata(root.join(source))?;
    let current = match std::fs::symlink_metadata(root.join(target)) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if current.as_ref().is_some_and(|m| m.is_dir()) && !incoming.is_dir() {
        return Err(AppError::validation(format!(
            "restore entry {} conflicts with a retained directory; project left untouched",
            target.display()
        )));
    }
    if incoming.is_dir() && current.as_ref().is_some_and(|m| m.is_dir()) {
        for entry in std::fs::read_dir(root.join(source))? {
            let name = entry?.file_name();
            merge_retained(
                root,
                directory,
                &source.join(&name),
                &target.join(name),
                actions,
            )?;
        }
    } else {
        if current.is_some() {
            save_old(root, directory, target, actions)?;
        }
        install(root, source, target, actions)?;
    }
    Ok(())
}

/// Return whether a conditional move already happened (including a lost ack).
fn position(root: &Path, action: &Action) -> AppResult<bool> {
    let source = receipt::identity(&root.join(&action.source))?;
    let target = receipt::identity(&root.join(&action.target))?;
    if source.as_ref() == Some(&action.identity) && target.is_none() {
        return Ok(false);
    }
    if source.is_none() && target.as_ref() == Some(&action.identity) {
        return Ok(true);
    }
    Err(AppError::system(format!(
        "restore action identity is unconfirmed: {} -> {}; evidence retained",
        action.source.display(),
        action.target.display()
    )))
}

fn move_entry(root: &Path, source: &Path, target: &Path, id: &receipt::Identity) -> AppResult<()> {
    if receipt::identity(&root.join(source))?.as_ref() != Some(id)
        || receipt::identity(&root.join(target))?.is_some()
    {
        return Err(AppError::system(
            "restore move identities changed; evidence retained",
        ));
    }
    #[cfg(unix)]
    {
        let source = crate::path_safety::ScopedParent::capture(root, source, false)?
            .ok_or_else(|| AppError::system("restore source parent disappeared"))?;
        let target = crate::path_safety::ScopedParent::capture(root, target, false)?
            .ok_or_else(|| AppError::system("restore destination parent disappeared"))?;
        let actual = source
            .leaf_identity()?
            .map(|(device, inode)| receipt::Identity::Unix { device, inode });
        if actual.as_ref() != Some(id) || target.leaf_exists()? {
            return Err(AppError::system(
                "captured restore rename identities changed; evidence retained",
            ));
        }
        source.rename_to_noreplace(&target)?;
        let moved = target
            .leaf_identity()?
            .map(|(device, inode)| receipt::Identity::Unix { device, inode });
        if moved.as_ref() != Some(id) {
            return Err(AppError::system(
                "captured moved restore identity changed; evidence retained",
            ));
        }
        source.sync_parent()?;
        target.sync_parent()?;
    }
    #[cfg(not(unix))]
    {
        // Windows retains checked identities and cooperative project locking.
        // FD-relative ancestor protection is a Unix capability, not claimed here.
        let source = crate::path_safety::ensure_within_path(root, source)?;
        let target = crate::path_safety::ensure_within_path(root, target)?;
        std::fs::rename(source, target)?;
    }
    #[cfg(not(unix))]
    if receipt::identity(&root.join(target))?.as_ref() != Some(id) {
        return Err(AppError::system(
            "moved restore object identity changed; evidence retained",
        ));
    }
    Ok(())
}

pub(super) fn apply(root: &Path, active: &mut Active, receipt: &mut Receipt) -> AppResult<()> {
    apply_with_hook(root, active, receipt, |_| Ok(()))
}
pub(super) fn apply_with_hook(
    root: &Path,
    active: &mut Active,
    receipt: &mut Receipt,
    mut hook: impl FnMut(usize) -> AppResult<()>,
) -> AppResult<()> {
    receipt::validate(root, active, receipt)?;
    while receipt.cursor < receipt.actions.len() {
        receipt::ensure_root(root, &receipt.root)?;
        let action = &receipt.actions[receipt.cursor];
        if !position(root, action)? {
            move_entry(root, &action.source, &action.target, &action.identity)?;
        }
        receipt.cursor += 1;
        receipt::save_progress(root, active, receipt)?;
        hook(receipt.cursor)?;
    }
    receipt.state = State::Committed;
    receipt::save_progress(root, active, receipt)
}

pub(super) fn rollback(root: &Path, active: &mut Active, receipt: &mut Receipt) -> AppResult<()> {
    receipt::validate(root, active, receipt)?;
    // A crash may fall between rename and its durable progress update.
    if receipt.state == State::Applying
        && receipt.cursor < receipt.actions.len()
        && position(root, &receipt.actions[receipt.cursor])?
    {
        receipt.cursor += 1;
    }
    receipt.state = State::RollingBack;
    receipt::save_progress(root, active, receipt)?;
    while receipt.cursor > 0 {
        receipt::ensure_root(root, &receipt.root)?;
        let action = &receipt.actions[receipt.cursor - 1];
        if position(root, action)? {
            move_entry(root, &action.target, &action.source, &action.identity)?;
        }
        receipt.cursor -= 1;
        receipt::save_progress(root, active, receipt)?;
    }
    Ok(())
}

pub(super) fn cleanup(root: &Path, active: &mut Active) -> AppResult<()> {
    cleanup_with_hook(root, active, |_| Ok(()))
}

pub(super) fn cleanup_with_hook(
    root: &Path,
    active: &mut Active,
    mut before_remove: impl FnMut(&Path) -> AppResult<()>,
) -> AppResult<()> {
    let plan = receipt::load_plan(root, active)?;
    if !(plan.state == State::Committed && plan.cursor == plan.actions.len())
        && !(plan.state == State::RollingBack && plan.cursor == 0)
    {
        return Err(AppError::system(
            "restore cleanup is not authorized by a terminal transaction; originals retained",
        ));
    }
    receipt::ensure_root(root, &active.root)?;
    let current = receipt::read_json::<Active>(root, Path::new(receipt::ACTIVE), 64 * 1024)?
        .ok_or_else(|| AppError::system("restore active pointer disappeared; evidence retained"))?;
    if current != *active {
        return Err(AppError::system(
            "restore pointer changed; evidence retained",
        ));
    }
    let original = Path::new(&active.directory);
    if active.cleanup.is_none() || receipt::identity(&root.join(original))?.is_some() {
        // A terminal label/cursor is not evidence that the physical moves
        // completed. A persisted cleanup name is not evidence either until the
        // captured preparation has actually moved into quarantine. Recheck the
        // full graph after a crash between authorization and that move.
        for action in &plan.actions {
            let path = match plan.state {
                State::Committed => &action.target,
                State::RollingBack => &action.source,
                State::Applying => {
                    return Err(AppError::system(
                        "applying restore cannot authorize cleanup",
                    ));
                }
            };
            #[cfg(unix)]
            let actual = match crate::path_safety::ScopedParent::capture(root, path, false)? {
                Some(parent) => parent
                    .leaf_identity()?
                    .map(|(device, inode)| receipt::Identity::Unix { device, inode }),
                None => None,
            };
            #[cfg(not(unix))]
            let actual = receipt::identity(&root.join(path))?;
            if actual.as_ref() != Some(&action.identity) {
                return Err(AppError::system(format!(
                    "restore terminal physical identity is unconfirmed for {}; originals retained",
                    path.display()
                )));
            }
        }
        if active.cleanup.is_none() {
            active.cleanup = Some(format!(
                "{}{}",
                receipt::PREFIX,
                uuid::Uuid::now_v7().simple()
            ));
            receipt::write_json(root, Path::new(receipt::ACTIVE), active)?;
        }
    }
    let cleanup = active
        .cleanup
        .as_deref()
        .ok_or_else(|| AppError::system("restore cleanup target missing"))?;
    let quarantined = Path::new(cleanup);
    match (
        receipt::identity(&root.join(original))?,
        receipt::identity(&root.join(quarantined))?,
    ) {
        (Some(id), None) if id == active.identity => move_entry(root, original, quarantined, &id)?,
        (None, Some(id)) if id == active.identity => {}
        (None, None) => {}
        _ => {
            return Err(AppError::system(
                "restore cleanup identities changed; evidence retained",
            ));
        }
    }
    let path = root.join(quarantined);
    before_remove(&path)?;
    // Never let a TempDir destructor own the only copies of original content.
    // This directory is removed only after a confirmed commit or full rollback.
    if let Some(id) = receipt::identity(&path)? {
        if id != active.identity {
            return Err(AppError::system(
                "restore cleanup directory identity changed; originals retained",
            ));
        }
        if !std::fs::symlink_metadata(&path)?.is_dir() {
            return Err(AppError::system(
                "restore cleanup object is not a directory",
            ));
        }
        super::cleanup::remove_preparation(root, quarantined, &active.identity)?;
    }
    let plan = receipt::plan_path(active);
    if let Some(id) = receipt::identity(&root.join(&plan))? {
        if id != active.plan_identity {
            return Err(AppError::system(
                "restore plan identity changed before cleanup",
            ));
        }
        remove_metadata(root, &plan)?;
    }
    #[cfg(unix)]
    {
        let parent =
            crate::path_safety::ScopedParent::capture(root, Path::new(receipt::ACTIVE), false)?
                .ok_or_else(|| AppError::system("restore pointer parent disappeared"))?;
        parent.remove_file()?;
    }
    #[cfg(not(unix))]
    std::fs::remove_file(root.join(receipt::ACTIVE))?;
    Ok(())
}

fn remove_metadata(root: &Path, relative: &Path) -> AppResult<()> {
    #[cfg(unix)]
    {
        if let Some(parent) = crate::path_safety::ScopedParent::capture(root, relative, false)? {
            parent.remove_file()?;
        }
    }
    #[cfg(not(unix))]
    std::fs::remove_file(root.join(relative))?;
    Ok(())
}

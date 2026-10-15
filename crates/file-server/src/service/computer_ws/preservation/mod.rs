//! Identity-bound recovery of locked skills. No receipt grants deletion rights
//! over an unverified directory, and incomplete copies are never discarded.

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::{AppError, AppResult};

use super::DYNAMIC_ADD_LOCK;
use lock::{DirectoryIdentity, checked_directory, identity};
use receipt::{Phase, Receipt, Skill, SkillPhase, SourceLayout, StoredReceipt};

mod lock;
mod receipt;
mod receipt_io;

pub(super) use lock::{WorkspaceGuard, acquire};

#[cfg(test)]
use receipt::LegacyReceipt as PreserveReceipt;

#[cfg(test)]
const PRESERVE_RECEIPT_VERSION: u32 = 1;

#[cfg(test)]
fn publish_preserve_receipt<F>(path: &Path, write: F) -> AppResult<()>
where
    F: FnOnce(&mut std::fs::File) -> std::io::Result<()>,
{
    receipt::publish(path, write)
}

#[cfg(test)]
async fn persist_receipt(workspace: &Path, value: &PreserveReceipt) -> AppResult<()> {
    receipt::validate_names(value.skills.iter().map(String::as_str))?;
    receipt::write_json(workspace, &preserve_receipt_path(workspace), value)
}

#[cfg(test)]
fn preserved_skill_path(workspace: &Path, name: &str) -> PathBuf {
    match receipt::read(workspace, &preserve_receipt_path(workspace)).expect("read test receipt") {
        Some(StoredReceipt::Current(value)) => {
            let skill = value
                .skills
                .iter()
                .find(|skill| skill.name == name)
                .expect("test skill");
            source_path(workspace, &value, skill)
        }
        Some(StoredReceipt::Legacy(_, _)) => preserve_area(workspace).join(name),
        None => panic!("test preserve receipt missing"),
    }
}

fn preserve_area(workspace: &Path) -> PathBuf {
    workspace.join(".agents/.preserved-skills")
}

fn preserve_receipt_path(workspace: &Path) -> PathBuf {
    preserve_area(workspace).join("receipt.json")
}

fn source_path(workspace: &Path, receipt: &Receipt, skill: &Skill) -> PathBuf {
    match skill.source_layout {
        SourceLayout::Operation => preserve_area(workspace)
            .join(&receipt.data_dir)
            .join(&skill.name),
        SourceLayout::Legacy => preserve_area(workspace).join(&skill.name),
    }
}

fn target_path(workspace: &Path, skill: &Skill) -> PathBuf {
    workspace.join(".agents/skills").join(&skill.name)
}

fn optional_identity(path: &Path) -> AppResult<Option<DirectoryIdentity>> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => identity(path, true).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::system(format!(
            "inspect {}: {error}",
            path.display()
        ))),
    }
}

async fn move_exact_directory(
    workspace: &Path,
    source: &Path,
    target: &Path,
    expected: &DirectoryIdentity,
) -> AppResult<()> {
    let root = workspace.to_path_buf();
    let source = source.to_path_buf();
    let target = target.to_path_buf();
    let expected = expected.clone();
    tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        {
            use crate::path_safety::ScopedParent;
            let source_relative = source.strip_prefix(&root).map_err(|error| {
                AppError::system(format!("preserved skill source scope: {error}"))
            })?;
            let target_relative = target.strip_prefix(&root).map_err(|error| {
                AppError::system(format!("preserved skill target scope: {error}"))
            })?;
            let from = ScopedParent::capture(&root, source_relative, false)?
                .ok_or_else(|| AppError::system("preserved skill source parent disappeared"))?;
            let to = ScopedParent::capture(&root, target_relative, false)?
                .ok_or_else(|| AppError::system("preserved skill target parent disappeared"))?;
            #[cfg(test)]
            test_gate::after_rename_capture(&root)?;
            let opened = from.open_directory()?;
            if lock::file_identity(&opened)? != expected || to.leaf_exists()? {
                return Err(AppError::system(
                    "preserved skill or its target changed before rename",
                ));
            }
            from.rename_to_noreplace(&to).map_err(|error| {
                AppError::system(format!(
                    "move locked skill {} to {}: {error}; recovery evidence retained",
                    source.display(),
                    target.display()
                ))
            })?;
            from.sync_parent()?;
            to.sync_parent()?;
            if lock::file_identity(&to.open_directory()?)? != expected {
                return Err(AppError::system(
                    "moved skill identity is unconfirmed; receipt retained",
                ));
            }
            // Never claim that the caller's pathname still identifies this
            // result when an ancestor has changed during the captured move.
            let current_from = ScopedParent::capture(&root, source_relative, false)?
                .ok_or_else(|| AppError::system("skill source binding changed after rename"))?;
            let current_to = ScopedParent::capture(&root, target_relative, false)?
                .ok_or_else(|| AppError::system("skill target binding changed after rename"))?;
            if !from.same_parent(&current_from)?
                || !to.same_parent(&current_to)?
                || lock::file_identity(&current_to.open_directory()?)? != expected
            {
                return Err(AppError::system(
                    "skill pathname binding changed; moved data and receipt retained",
                ));
            }
        }
        #[cfg(not(unix))]
        {
            checked_directory(
                &root,
                source
                    .parent()
                    .ok_or_else(|| AppError::system("preserved skill has no source parent"))?,
            )?;
            checked_directory(
                &root,
                target
                    .parent()
                    .ok_or_else(|| AppError::system("preserved skill has no target parent"))?,
            )?;
            if optional_identity(&source)?.as_ref() != Some(&expected)
                || optional_identity(&target)?.is_some()
            {
                return Err(AppError::system(
                    "preserved skill or its target changed before rename",
                ));
            }
            std::fs::rename(&source, &target).map_err(|error| {
                AppError::system(format!(
                    "move locked skill {} to {}: {error}; recovery evidence retained",
                    source.display(),
                    target.display()
                ))
            })?;
            if identity(&target, true)? != expected {
                return Err(AppError::system(
                    "moved skill identity is unconfirmed; receipt retained",
                ));
            }
        }
        Ok(())
    })
    .await
    .map_err(|error| AppError::system(format!("skill rename worker interrupted: {error}")))?
}

fn validate_binding(workspace: &Path, value: &Receipt) -> AppResult<()> {
    receipt::validate_names(value.skills.iter().map(|skill| skill.name.as_str()))?;
    receipt::validate_name(&value.data_dir)?;
    let canonical = std::fs::canonicalize(workspace)?;
    if value.workspace_root != canonical.to_string_lossy()
        || value.workspace_identity != identity(&canonical, true)?
    {
        return Err(AppError::system(
            "preserve receipt belongs to a different workspace identity; receipt and copies retained",
        ));
    }
    let area = preserve_area(workspace);
    checked_directory(&canonical, &area)?;
    let data = area.join(&value.data_dir);
    if identity(&data, true)? != value.data_identity {
        return Err(AppError::system(
            "preserve data directory identity changed; receipt and copies retained",
        ));
    }
    Ok(())
}

fn save_owned(workspace: &Path, value: &mut Receipt) -> AppResult<()> {
    validate_binding(workspace, value)?;
    let path = preserve_receipt_path(workspace);
    let captured = receipt::Captured::capture(workspace, &path)?
        .ok_or_else(|| AppError::system("preserve receipt parent disappeared"))?;
    let Some(StoredReceipt::Current(current)) = captured.read()? else {
        return Err(AppError::system(
            "preserve operation receipt disappeared or changed version",
        ));
    };
    if current.operation_id != value.operation_id
        || current.data_dir != value.data_dir
        || current.data_identity != value.data_identity
        || current.revision != value.revision
    {
        return Err(AppError::system(
            "preserve operation identity or revision changed; refusing late publication",
        ));
    }
    value.revision = value
        .revision
        .checked_add(1)
        .ok_or_else(|| AppError::system("preserve receipt revision overflow"))?;
    captured.write_json(value)
}

fn new_receipt(workspace: &Path, operation_id: String, skills: Vec<Skill>) -> AppResult<Receipt> {
    let canonical = std::fs::canonicalize(workspace)?;
    let area = preserve_area(&canonical);
    std::fs::create_dir_all(&area)?;
    checked_directory(&canonical, &area)?;
    let data_dir = format!(".operation-{}", uuid::Uuid::now_v7().simple());
    let data = area.join(&data_dir);
    std::fs::create_dir(&data)?;
    Ok(Receipt {
        version: receipt::VERSION,
        operation_id,
        workspace_root: canonical.to_string_lossy().into_owned(),
        workspace_identity: identity(&canonical, true)?,
        data_dir,
        data_identity: identity(&data, true)?,
        revision: 0,
        phase: Phase::Preserving,
        skills,
    })
}

fn migrate_legacy(
    workspace: &Path,
    legacy: receipt::LegacyReceipt,
    original: &[u8],
) -> AppResult<Receipt> {
    use sha2::Digest as _;
    receipt::validate_names(legacy.skills.iter().map(String::as_str))?;
    let canonical = std::fs::canonicalize(workspace)?;
    if legacy.workspace_root != canonical.to_string_lossy() {
        return Err(AppError::system(
            "legacy preserve receipt belongs to another workspace",
        ));
    }
    checked_directory(&canonical, &preserve_area(workspace))?;
    let mut skills = Vec::with_capacity(legacy.skills.len());
    for name in &legacy.skills {
        let source = preserve_area(workspace).join(name);
        let target = workspace.join(".agents/skills").join(name);
        let captured = match (optional_identity(&source)?, optional_identity(&target)?) {
            (Some(id), None) | (None, Some(id)) => id,
            (Some(_), Some(_)) => {
                return Err(AppError::system(format!(
                    "legacy locked skill {name:?} has two unproven copies at {} and {}; both copies and original receipt retained",
                    source.display(),
                    target.display()
                )));
            }
            (None, None) => {
                return Err(AppError::system(format!(
                    "legacy locked skill {name:?} is unavailable at {} and {}; original receipt retained",
                    source.display(),
                    target.display()
                )));
            }
        };
        skills.push(Skill {
            name: name.clone(),
            identity: captured,
            phase: SkillPhase::Planned,
            source_layout: SourceLayout::Legacy,
        });
    }
    let history = workspace.join(".agents/.skill-preservation-history");
    std::fs::create_dir_all(&history)?;
    checked_directory(&canonical, &history)?;
    let backup = history.join(format!(
        "legacy-{}.json",
        hex::encode(sha2::Sha256::digest(original))
    ));
    match receipt::read_bytes(workspace, &backup)? {
        Some(existing) if existing == original => {}
        Some(_) => {
            return Err(AppError::system(
                "legacy preserve receipt backup conflicts; originals retained",
            ));
        }
        None => receipt::write_bytes(workspace, &backup, original)?,
    }
    if receipt::read_bytes(workspace, &preserve_receipt_path(workspace))?.as_deref()
        != Some(original)
    {
        return Err(AppError::system(
            "legacy preserve receipt changed before migration",
        ));
    }
    let value = new_receipt(workspace, legacy.operation_id, skills)?;
    receipt::write_json(workspace, &preserve_receipt_path(workspace), &value)?;
    Ok(value)
}

async fn restore_entries(workspace: &Path, value: &mut Receipt) -> AppResult<()> {
    validate_binding(workspace, value)?;
    // Inspect the entire set before advancing the durable phase or moving an
    // earlier valid item. A malformed or conflicting later item is not success.
    for skill in &value.skills {
        let source = source_path(workspace, value, skill);
        let target = target_path(workspace, skill);
        match (optional_identity(&source)?, optional_identity(&target)?) {
            (Some(id), None) | (None, Some(id)) if id == skill.identity => {}
            _ => {
                return Err(AppError::system(format!(
                    "locked skill {:?} is unavailable, conflicting, or changed identity at {} and {}; copies and receipt retained",
                    skill.name,
                    source.display(),
                    target.display()
                )));
            }
        }
    }
    fs::create_dir_all(workspace.join(".agents/skills")).await?;
    checked_directory(workspace, &workspace.join(".agents/skills"))?;
    value.phase = Phase::Restoring;
    save_owned(workspace, value)?;
    for index in 0..value.skills.len() {
        validate_binding(workspace, value)?;
        let skill = &value.skills[index];
        let source = source_path(workspace, value, skill);
        let target = target_path(workspace, skill);
        match (optional_identity(&source)?, optional_identity(&target)?) {
            (Some(source_id), None) if source_id == skill.identity => {
                value.skills[index].phase = SkillPhase::RestoreAccepted;
                save_owned(workspace, value)?;
                let expected = value.skills[index].identity.clone();
                move_exact_directory(workspace, &source, &target, &expected).await?;
                if identity(&target, true)? != value.skills[index].identity {
                    return Err(AppError::system(format!(
                        "restored locked skill {:?} changed identity; receipt retained",
                        value.skills[index].name
                    )));
                }
            }
            (None, Some(target_id)) if target_id == skill.identity => {}
            (Some(_), Some(_)) => {
                return Err(AppError::system(format!(
                    "locked skill {:?} conflicts at {} and {}; target ownership unconfirmed; both copies and receipt retained",
                    skill.name,
                    source.display(),
                    target.display()
                )));
            }
            _ => {
                return Err(AppError::system(format!(
                    "locked skill {:?} is missing or changed identity at {} and {}; recovery evidence retained",
                    skill.name,
                    source.display(),
                    target.display()
                )));
            }
        }
        value.skills[index].phase = SkillPhase::Restored;
        save_owned(workspace, value)?;
    }
    value.phase = Phase::Restored;
    save_owned(workspace, value)?;
    confirm_finished(workspace, value).await
}

async fn confirm_finished(workspace: &Path, value: &Receipt) -> AppResult<()> {
    validate_binding(workspace, value)?;
    let captured_receipt =
        receipt::Captured::capture(workspace, &preserve_receipt_path(workspace))?.ok_or_else(
            || AppError::system("preserve receipt parent disappeared before confirmation"),
        )?;
    let Some(StoredReceipt::Current(current)) = captured_receipt.read()? else {
        return Err(AppError::system(
            "preserve receipt changed before confirmation",
        ));
    };
    if current.operation_id != value.operation_id
        || current.data_identity != value.data_identity
        || current.data_dir != value.data_dir
        || current.revision != value.revision
        || current.phase != Phase::Restored
    {
        return Err(AppError::system(
            "preserve confirmation belongs to a different operation",
        ));
    }
    for skill in &value.skills {
        if optional_identity(&source_path(workspace, value, skill))?.is_some()
            || optional_identity(&target_path(workspace, skill))?.as_ref() != Some(&skill.identity)
            || skill.phase != SkillPhase::Restored
        {
            return Err(AppError::system(
                "locked skill completion is unconfirmed; receipt retained",
            ));
        }
    }
    // Only empty owned directories can be cleaned. Unknown files are retained.
    let data = preserve_area(workspace).join(&value.data_dir);
    #[cfg(unix)]
    {
        use crate::path_safety::ScopedParent;
        use rustix::fs::{Mode, OFlags};
        let relative = data
            .strip_prefix(workspace)
            .map_err(|error| AppError::system(format!("preserve data scope: {error}")))?;
        let data_parent = ScopedParent::capture(workspace, relative, false)?
            .ok_or_else(|| AppError::system("preserve data parent disappeared before cleanup"))?;
        let opened = data_parent.open_directory()?;
        if lock::file_identity(&opened)? != value.data_identity {
            return Err(AppError::system(
                "preserve data identity changed before cleanup",
            ));
        }
        let readable = rustix::fs::openat(
            &opened,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        let mut entries = rustix::fs::Dir::read_from(&readable).map_err(std::io::Error::from)?;
        for entry in &mut entries {
            let entry = entry.map_err(std::io::Error::from)?;
            if !matches!(entry.file_name().to_bytes(), b"." | b"..") {
                return Err(AppError::system(
                    "preserve data contains unconfirmed entries; receipt retained",
                ));
            }
        }
        captured_receipt.check_binding()?;
        captured_receipt.remove()?;
        if lock::file_identity(&data_parent.open_directory()?)? != value.data_identity {
            return Err(AppError::system(
                "confirmed preserve data identity changed; directory retained",
            ));
        }
        if let Err(error) = data_parent.remove_directory() {
            tracing::warn!(%error, data = %data.display(), "empty confirmed preserve directory retained");
        }
        let area_relative = preserve_area(workspace)
            .strip_prefix(workspace)
            .map_err(|error| AppError::system(format!("preserve area scope: {error}")))?
            .to_owned();
        if let Some(area_parent) = ScopedParent::capture(workspace, &area_relative, false)?
            && let Err(error) = area_parent.remove_directory()
        {
            tracing::warn!(%error, "confirmed preserve area has retained entries");
        }
    }
    #[cfg(not(unix))]
    {
        if std::fs::read_dir(&data)?.next().is_some() {
            return Err(AppError::system(
                "preserve data contains unconfirmed entries; receipt retained",
            ));
        }
        captured_receipt.check_binding()?;
        captured_receipt.remove()?;
        if identity(&data, true)? != value.data_identity {
            return Err(AppError::system(
                "confirmed preserve data identity changed; directory retained",
            ));
        }
        if let Err(error) = fs::remove_dir(&data).await {
            tracing::warn!(%error, data = %data.display(), "empty confirmed preserve directory retained");
        }
        if let Err(error) = fs::remove_dir(preserve_area(workspace)).await {
            tracing::warn!(%error, "confirmed preserve area has retained entries");
        }
    }
    Ok(())
}

pub(super) async fn resume_unfinished_preservation(workspace: &Path) -> AppResult<()> {
    let stored = receipt::read(workspace, &preserve_receipt_path(workspace))?;
    let mut value = match stored {
        None => return Ok(()),
        Some(StoredReceipt::Current(value)) => value,
        Some(StoredReceipt::Legacy(legacy, bytes)) => migrate_legacy(workspace, legacy, &bytes)?,
    };
    restore_entries(workspace, &mut value).await
}

pub(super) struct PreserveAreaHandle {
    operation_id: String,
}

pub(super) async fn preserve_locked_skills(
    skills_dir: &Path,
    workspace: &Path,
) -> AppResult<(Option<PreserveAreaHandle>, Vec<String>)> {
    let mut names = Vec::new();
    let mut skills = Vec::new();
    let mut entries = fs::read_dir(skills_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir()
            && fs::try_exists(entry.path().join(DYNAMIC_ADD_LOCK)).await?
        {
            let name = entry.file_name().into_string().map_err(|_| {
                AppError::system("locked skill name cannot be represented in its preserve receipt")
            })?;
            receipt::validate_name(&name)?;
            names.push(name.clone());
            skills.push(Skill {
                name,
                identity: identity(&entry.path(), true)?,
                phase: SkillPhase::Planned,
                source_layout: SourceLayout::Operation,
            });
        }
    }
    if names.is_empty() {
        return Ok((None, names));
    }
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    names.sort();
    if receipt::read(workspace, &preserve_receipt_path(workspace))?.is_some() {
        return Err(AppError::system(
            "previous skill preservation is unfinished; retry its recovery",
        ));
    }
    let mut value = new_receipt(workspace, uuid::Uuid::now_v7().simple().to_string(), skills)?;
    receipt::write_json(workspace, &preserve_receipt_path(workspace), &value)?;
    for index in 0..value.skills.len() {
        validate_binding(workspace, &value)?;
        let target = source_path(workspace, &value, &value.skills[index]);
        let source = target_path(workspace, &value.skills[index]);
        if optional_identity(&source)?.as_ref() != Some(&value.skills[index].identity)
            || optional_identity(&target)?.is_some()
        {
            return Err(AppError::system(
                "locked skill source identity changed before preservation",
            ));
        }
        let expected = value.skills[index].identity.clone();
        move_exact_directory(workspace, &source, &target, &expected).await?;
        if identity(&target, true)? != value.skills[index].identity {
            return Err(AppError::system(
                "preserved skill identity changed; both locations retained for inspection",
            ));
        }
        value.skills[index].phase = SkillPhase::Preserved;
        save_owned(workspace, &mut value)?;
    }
    value.phase = Phase::Preserved;
    save_owned(workspace, &mut value)?;
    Ok((
        Some(PreserveAreaHandle {
            operation_id: value.operation_id,
        }),
        names,
    ))
}

pub(super) fn before_rebuild(
    preserved: &(Option<PreserveAreaHandle>, Vec<String>),
    workspace: &Path,
) -> AppResult<()> {
    let Some(handle) = &preserved.0 else {
        return Ok(());
    };
    let Some(StoredReceipt::Current(mut value)) =
        receipt::read(workspace, &preserve_receipt_path(workspace))?
    else {
        return Err(AppError::system(
            "preserve receipt disappeared before workspace rebuild",
        ));
    };
    if value.operation_id != handle.operation_id || value.phase != Phase::Preserved {
        return Err(AppError::system(
            "workspace rebuild belongs to a different preservation",
        ));
    }
    validate_binding(workspace, &value)?;
    for skill in &value.skills {
        if optional_identity(&source_path(workspace, &value, skill))?.as_ref()
            != Some(&skill.identity)
            || optional_identity(&target_path(workspace, skill))?.is_some()
        {
            return Err(AppError::system(
                "locked skill preservation is unconfirmed; workspace not cleared",
            ));
        }
    }
    value.phase = Phase::Rebuilding;
    save_owned(workspace, &mut value)
}

pub(super) async fn restore_locked_skills(
    preserved: &(Option<PreserveAreaHandle>, Vec<String>),
    workspace: &Path,
) -> AppResult<()> {
    let Some(handle) = &preserved.0 else {
        return Ok(());
    };
    let Some(StoredReceipt::Current(mut value)) =
        receipt::read(workspace, &preserve_receipt_path(workspace))?
    else {
        return Err(AppError::system(
            "preserve receipt disappeared before restore",
        ));
    };
    if value.operation_id != handle.operation_id {
        return Err(AppError::system(
            "late skill restore belongs to a different operation",
        ));
    }
    restore_entries(workspace, &mut value).await
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(super) mod test_gate;

#[cfg(test)]
mod concurrency_tests;

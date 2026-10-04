//! File restoration transactions. All I/O and physical locks live in one
//! blocking worker, including cancellation, rollback and deferred cleanup.
mod cleanup;
mod commit;
mod receipt;
#[cfg(test)]
mod tests;

use crate::error::{AppError, AppResult};
use receipt::{Active, Identity, Receipt, State};

pub(super) fn protected_files() -> [String; 2] {
    [receipt::LOCK.to_owned(), receipt::ACTIVE.to_owned()]
}
use sha2::Digest as _;
use std::{
    fs::File,
    path::{Path, PathBuf},
};

pub(super) fn acquire_project_lock(root: &Path) -> AppResult<File> {
    let path = root.join(receipt::LOCK);
    #[cfg(unix)]
    let file = File::from(
        rustix::fs::open(
            &path,
            rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::from_raw_mode(0o600),
        )
        .map_err(std::io::Error::from)?,
    );
    #[cfg(not(unix))]
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    if !file.metadata()?.is_file() {
        return Err(AppError::system(
            "version restore lock is not a regular file",
        ));
    }
    file.lock()
        .map_err(|e| AppError::system(format!("lock version restore workspace: {e}")))?;
    if receipt::identity(&path)?.as_ref() != Some(&receipt::file_id(&file)?) {
        return Err(AppError::system("version restore lock identity changed"));
    }
    Ok(file)
}

pub(super) fn recover(root: &Path) -> AppResult<()> {
    let Some(mut active) =
        receipt::read_json::<Active>(root, Path::new(receipt::ACTIVE), 64 * 1024)?
    else {
        return Ok(());
    };
    if !receipt::valid_name(&active.directory)
        || active
            .cleanup
            .as_deref()
            .is_some_and(|name| !receipt::valid_name(name))
    {
        return Err(AppError::system(
            "invalid restore recovery directory; evidence retained",
        ));
    }
    receipt::ensure_root(root, &active.root)?;
    let preparation_missing = receipt::identity(&root.join(&active.directory))?.is_none()
        && match active.cleanup.as_deref() {
            Some(path) => receipt::identity(&root.join(path))?.is_none(),
            None => true,
        };
    let terminal = active.state == State::Committed
        || (active.state == State::RollingBack && active.cursor == 0);
    // Cleanup can be interrupted after deleting the plan but before retiring
    // its pointer. This retires metadata only; no business result is replayed.
    if terminal
        && preparation_missing
        && receipt::identity(&root.join(receipt::plan_path(&active)))?.is_none()
    {
        return remove_active_pointer(root);
    }
    let mut plan = receipt::load_plan(root, &active)?;
    match plan.state {
        State::Applying | State::RollingBack => commit::rollback(root, &mut active, &mut plan)?,
        State::Committed => {}
    }
    commit::cleanup(root, &mut active)
}

fn remove_active_pointer(root: &Path) -> AppResult<()> {
    #[cfg(unix)]
    {
        if let Some(parent) =
            crate::path_safety::ScopedParent::capture(root, Path::new(receipt::ACTIVE), false)?
        {
            parent.remove_file()?;
        }
    }
    #[cfg(not(unix))]
    std::fs::remove_file(root.join(receipt::ACTIVE))?;
    Ok(())
}

fn required_id(path: &Path) -> AppResult<Identity> {
    receipt::identity(path)?
        .ok_or_else(|| AppError::system(format!("restore object {} disappeared", path.display())))
}

fn prepare(
    root: &Path,
    source: &Path,
    dirs: &[String],
    files: &[String],
) -> AppResult<(Active, Receipt)> {
    let name = format!("{}{}", receipt::PREFIX, uuid::Uuid::now_v7().simple());
    let preparation = tempfile::Builder::new()
        .prefix(&name)
        .rand_bytes(0)
        .tempdir_in(root)?;
    let mut snapshot = tempfile::tempfile_in(preparation.path())?;
    crate::service::zip::capture_snapshot(source, &mut snapshot)?;
    crate::service::zip::validate_open_file(snapshot.try_clone()?).map_err(
        |error| match error {
            AppError::File(_) | AppError::Validation(_, _) => AppError::resource(format!(
                "restore source {} is corrupt ({error}); project left untouched",
                source.display()
            )),
            other => AppError::system(format!(
                "inspect restore source {}: {other}; project left untouched",
                source.display()
            )),
        },
    )?;
    let incoming = preparation.path().join("incoming");
    std::fs::create_dir(&incoming)?;
    crate::service::zip::extract_open_file(snapshot, &incoming)?;
    std::fs::create_dir(preparation.path().join("backup"))?;
    let root_id = required_id(root)?;
    let directory_id = required_id(preparation.path())?;
    let actions = commit::plan(root, &name, dirs, files)?;
    let receipt = Receipt {
        schema: 1,
        root: root_id.clone(),
        directory: directory_id.clone(),
        actions,
        cursor: 0,
        state: State::Applying,
    };
    let bytes = serde_json::to_vec(&receipt)
        .map_err(|e| AppError::system(format!("serialize restore plan: {e}")))?;
    if bytes.len() > 128 * 1024 * 1024 || receipt.actions.len() > 200_000 {
        return Err(AppError::resource(
            "restore plan exceeds bounded recovery budget; project left untouched",
        ));
    }
    let plan_path = PathBuf::from(format!("{name}.plan.json"));
    if receipt::identity(&root.join(&plan_path))?.is_some() {
        return Err(AppError::system(
            "restore plan namespace is occupied; project left untouched",
        ));
    }
    receipt::write_json(root, &plan_path, &receipt)?;
    let active = Active {
        directory: name,
        identity: directory_id,
        root: root_id,
        cleanup: None,
        plan_identity: required_id(&root.join(&plan_path))?,
        plan_hash: hex::encode(sha2::Sha256::digest(&bytes)),
        cursor: 0,
        state: State::Applying,
    };
    // Before publication this directory contains only an archive preparation.
    // After publication original objects may enter it, so RAII must not delete it.
    let _retained = preparation.keep();
    receipt::write_json(root, Path::new(receipt::ACTIVE), &active)?;
    Ok((active, receipt))
}

pub(super) fn run(root: &Path, source: &Path, dirs: &[String], files: &[String]) -> AppResult<()> {
    let root = std::fs::canonicalize(root)?;
    let _guard = acquire_project_lock(&root)?;
    recover(&root)?;
    let (mut active, mut plan) = prepare(&root, source, dirs, files)?;
    #[cfg(test)]
    let applied = {
        let fail = test_failure_roots().lock().unwrap().remove(&root);
        let last = plan.actions.len();
        if fail {
            commit::apply_with_hook(&root, &mut active, &mut plan, |cursor| {
                if fail && cursor == last {
                    Err(AppError::system("injected landing failure"))
                } else {
                    Ok(())
                }
            })
        } else {
            commit::apply(&root, &mut active, &mut plan)
        }
    };
    #[cfg(not(test))]
    let applied = commit::apply(&root, &mut active, &mut plan);
    if let Err(error) = applied {
        if let Err(rollback) = commit::rollback(&root, &mut active, &mut plan) {
            return Err(AppError::system(format!(
                "restore not committed: {error}; rollback unconfirmed: {rollback}; recovery evidence retained"
            )));
        }
        if let Err(cleanup) = commit::cleanup(&root, &mut active) {
            return Err(AppError::system(format!(
                "restore not committed: {error}; original files restored; cleanup unconfirmed: {cleanup}; recovery evidence retained"
            )));
        }
        return Err(AppError::system(format!(
            "restore not committed: {error}; original files restored"
        )));
    }
    commit::cleanup(&root, &mut active).map_err(|error| {
        AppError::system(format!(
            "restore committed; cleanup unconfirmed: {error}; recovery evidence retained"
        ))
    })
}

#[cfg(test)]
fn test_failure_roots() -> &'static std::sync::Mutex<std::collections::HashSet<PathBuf>> {
    static ROOTS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
        std::sync::OnceLock::new();
    ROOTS.get_or_init(Default::default)
}

#[cfg(test)]
pub(super) fn inject_landing_failure(root: &Path) {
    test_failure_roots()
        .lock()
        .unwrap()
        .insert(root.canonicalize().unwrap());
}

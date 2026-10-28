//! Request deadlines bound observation, never erase a dispatched supervisor write.
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU8, Ordering},
    },
};

use anyhow::{Context, Result};
use tokio::{
    sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock as AsyncRwLock},
    time::Instant,
};

type Fence = Arc<AsyncRwLock<()>>;
type FenceRegistry = Mutex<BTreeMap<std::path::PathBuf, Weak<AsyncRwLock<()>>>>;
static FENCES: OnceLock<FenceRegistry> = OnceLock::new();

const PENDING: u8 = 0;
const ATTEMPTING: u8 = 1;
const EXPIRED_BEFORE_DISPATCH: u8 = 2;

pub(super) struct DispatchState(AtomicU8);
impl DispatchState {
    pub(super) fn new() -> Self {
        Self(AtomicU8::new(PENDING))
    }

    /// The caller revokes Pending atomically. Once BeforeDispatch is returned,
    /// the retained task can never begin a socket write, even after rescheduling.
    pub(super) fn expire_pending(&self) -> Result<bool> {
        match self.0.compare_exchange(
            PENDING,
            EXPIRED_BEFORE_DISPATCH,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) | Err(EXPIRED_BEFORE_DISPATCH) => Ok(false),
            Err(ATTEMPTING) => Ok(true),
            Err(state) => anyhow::bail!("invalid supervisor dispatch state {state}"),
        }
    }

    #[cfg(unix)]
    pub(super) fn begin_attempt(&self) -> Result<()> {
        match self
            .0
            .compare_exchange(PENDING, ATTEMPTING, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Ok(()),
            Err(EXPIRED_BEFORE_DISPATCH) => Err(before_dispatch("revoked request")),
            Err(ATTEMPTING) => anyhow::bail!("supervisor mutation attempted twice"),
            Err(state) => anyhow::bail!("invalid supervisor dispatch state {state}"),
        }
    }

    pub(super) fn attempting(&self) -> bool {
        self.0.load(Ordering::Acquire) == ATTEMPTING
    }
}

pub(super) struct CallerDispatchGuard(pub(super) Arc<DispatchState>);
impl Drop for CallerDispatchGuard {
    fn drop(&mut self) {
        if let Err(error) = self.0.expire_pending() {
            tracing::error!(error = %error, "supervisor caller dispatch revocation failed");
        }
    }
}

/// All clients of the same physical socket share the fence, including legacy
/// APIs. Start/config writes are exclusive; precise cleanup commands share read
/// permits so different business groups retain their concurrent stop grace.
pub(super) fn fence(socket: &Path) -> Result<Fence> {
    anyhow::ensure!(
        socket.is_absolute()
            && !socket
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir)),
        "supervisor mutation socket must have a stable absolute path"
    );
    match std::fs::symlink_metadata(socket) {
        Ok(metadata) => anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "supervisor mutation socket may not be a symbolic link"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("observe supervisor mutation socket"),
    }
    // Resolve the parent only. Resolving the socket itself switches keys when
    // a pathname is missing, created, renamed, or replaced during a late RPC.
    let parent = socket
        .parent()
        .context("supervisor mutation socket has no parent")?;
    let name = socket
        .file_name()
        .context("supervisor mutation socket has no filename")?;
    let key = match std::fs::canonicalize(parent) {
        Ok(parent) => parent.join(name),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => socket.to_path_buf(),
        Err(error) => return Err(error).context("resolve supervisor mutation socket parent"),
    };
    let mut registry = FENCES
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .map_err(|_| anyhow::anyhow!("supervisor mutation fence registry poisoned"))?;
    registry.retain(|_, fence| fence.strong_count() != 0);
    // Retain the original absolute alias while the writer is alive. If its
    // parent is renamed/deleted, a cleanup lookup cannot create a second fence.
    if let Some(existing) = registry.get(socket).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    let fence = match registry.entry(key) {
        std::collections::btree_map::Entry::Occupied(mut entry) => match entry.get().upgrade() {
            Some(fence) => fence,
            None => {
                // The final wire task can settle concurrently with this short
                // registry lock. Its dead Weak is no ongoing-write evidence.
                let fence = Arc::new(AsyncRwLock::new(()));
                entry.insert(Arc::downgrade(&fence));
                fence
            }
        },
        std::collections::btree_map::Entry::Vacant(entry) => {
            let fence = Arc::new(AsyncRwLock::new(()));
            entry.insert(Arc::downgrade(&fence));
            fence
        }
    };
    registry.insert(socket.to_path_buf(), Arc::downgrade(&fence));
    Ok(fence)
}

#[cfg(unix)]
pub(super) fn pending(socket: &Path) -> Result<bool> {
    let fence = fence(socket)?;
    Ok(fence.try_write().is_err())
}

/// Only stop/remove may use a shared permit. Both permit kinds keep startup and
/// owner retirement excluded until the retained RPC's physical reply settles.
pub(super) enum MutationPermit {
    Exclusive(OwnedRwLockWriteGuard<()>),
    Cleanup(OwnedRwLockReadGuard<()>),
}

impl MutationPermit {
    pub(super) fn release(self) {
        match self {
            Self::Exclusive(guard) => drop(guard),
            Self::Cleanup(guard) => drop(guard),
        }
    }
}

#[derive(Debug)]
pub(crate) struct BeforeDispatch(pub(crate) String);
impl std::fmt::Display for BeforeDispatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for BeforeDispatch {}

pub(super) fn before_dispatch(method: &str) -> anyhow::Error {
    BeforeDispatch(format!(
        "supervisord {method} deadline exhausted before dispatch"
    ))
    .into()
}

pub(super) fn require_budget(method: &str, deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        Err(before_dispatch(method))
    } else {
        Ok(())
    }
}

#[cfg(unix)]
pub(super) async fn wait(socket: &Path, deadline: Instant) -> Result<()> {
    let fence = fence(socket)?;
    let pending = tokio::time::timeout_at(deadline, fence.write_owned())
        .await
        .map_err(|_| {
            crate::supervisor::ShutdownUnconfirmed(
                "supervisord dispatched mutation has not settled before cleanup deadline".into(),
            )
        })?;
    drop(pending);
    Ok(())
}

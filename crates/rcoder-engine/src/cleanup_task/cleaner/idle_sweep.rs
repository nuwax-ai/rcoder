//! In-memory grace for consecutive, independent cleanup sweeps.
use shared_types::ContainerBasicInfo;
use std::collections::HashSet;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct IdleWorkload {
    name: String,
    container_id: String,
    workload_uid: Option<String>,
}

impl From<&ContainerBasicInfo> for IdleWorkload {
    fn from(container: &ContainerBasicInfo) -> Self {
        Self {
            name: container.container_name.clone(),
            container_id: container.container_id.clone(),
            workload_uid: container.workload_uid.clone(),
        }
    }
}

pub(super) enum IdleConfirmation {
    Pending,
    Confirmed,
}

pub(super) struct IdleSweep {
    previous: HashSet<IdleWorkload>,
    observed: HashSet<IdleWorkload>,
    pending: HashSet<IdleWorkload>,
}

impl IdleSweep {
    pub(super) fn new(previous: HashSet<IdleWorkload>) -> Self {
        Self {
            previous,
            observed: HashSet::new(),
            pending: HashSet::new(),
        }
    }

    /// Deduplicate even failed/skipped attempts: another shared project must
    /// not retry an unconfirmed physical stop within the same sweep.
    pub(super) fn observe_once(&mut self, container: &ContainerBasicInfo) -> bool {
        self.observed.insert(container.into())
    }

    pub(super) fn confirm(&mut self, container: &ContainerBasicInfo) -> IdleConfirmation {
        let key = IdleWorkload::from(container);
        if self.previous.contains(&key) {
            IdleConfirmation::Confirmed
        } else {
            self.pending.insert(key);
            IdleConfirmation::Pending
        }
    }

    pub(super) fn into_pending(self) -> HashSet<IdleWorkload> {
        self.pending
    }
}

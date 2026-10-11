//! Cancel exactly the current Source's child token, while its driver retains
//! responsibility for joining commands and proving cleanup before a terminal.
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, ensure};
use tokio_util::sync::CancellationToken;

#[derive(Default, Clone)]
pub(super) struct SourceCancellation {
    current: Arc<Mutex<Option<RegisteredSource>>>,
}

struct RegisteredSource {
    ticket: String,
    operation_id: String,
    token: CancellationToken,
}

pub(super) struct SourceRegistration {
    registry: SourceCancellation,
    ticket: String,
}

impl SourceCancellation {
    fn register(&self, operation_id: &str, token: CancellationToken) -> Result<SourceRegistration> {
        let mut slot = self
            .current
            .lock()
            .map_err(|_| anyhow::anyhow!("Source cancellation registry lock poisoned"))?;
        ensure!(
            slot.is_none(),
            "a Source execution still owns the cancellation registry"
        );
        let ticket = uuid::Uuid::new_v4().to_string();
        *slot = Some(RegisteredSource {
            ticket: ticket.clone(),
            operation_id: operation_id.to_owned(),
            token,
        });
        Ok(SourceRegistration {
            registry: self.clone(),
            ticket,
        })
    }

    /// Synchronous notification only. In particular, no kernel calls, desired
    /// writes or operation finish may run under the kernel's admission lock.
    pub(super) fn cancel(&self, operation_id: &str) -> Result<bool> {
        let slot = self
            .current
            .lock()
            .map_err(|_| anyhow::anyhow!("Source cancellation registry lock poisoned"))?;
        let Some(source) = slot
            .as_ref()
            .filter(|source| source.operation_id == operation_id)
        else {
            return Ok(false);
        };
        source.token.cancel();
        Ok(true)
    }
}

impl Drop for SourceRegistration {
    fn drop(&mut self) {
        match self.registry.current.lock() {
            Ok(mut slot)
                if slot
                    .as_ref()
                    .is_some_and(|source| source.ticket == self.ticket) =>
            {
                *slot = None;
            }
            Ok(_) => {}
            Err(_) => tracing::error!("Source cancellation registration could not be released"),
        }
    }
}

impl super::ServerState {
    pub(super) fn register_source_cancellation(
        &self,
        token: CancellationToken,
    ) -> Result<Option<SourceRegistration>> {
        let Some(operation_id) = self.current_runtime_operation() else {
            return Ok(None);
        };
        let Some(kernel) = self.runtime_kernel() else {
            return Ok(None);
        };
        if !kernel.is_source_operation(&operation_id)? {
            return Ok(None);
        }
        let registration = self
            .source_cancellation
            .register(&operation_id, token)
            .context("register original Source execution cancellation")?;
        // A newer Source may have persisted cancellation before this driver
        // registered its token. Observe after registration, without holding the
        // registry mutex, to close that window without a reversed lock order.
        if kernel.is_cancelled(&operation_id) {
            self.source_cancellation.cancel(&operation_id)?;
        }
        Ok(Some(registration))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_cancels_only_registered_operation_and_guard_cannot_clear_successor() {
        let registry = SourceCancellation::default();
        let old = CancellationToken::new();
        let original = registry.register("source-old", old.clone()).unwrap();
        assert!(!registry.cancel("source-new").unwrap());
        assert!(!old.is_cancelled());
        assert!(registry.cancel("source-old").unwrap());
        assert!(old.is_cancelled());
        assert!(
            registry
                .register("source-new", CancellationToken::new())
                .is_err()
        );
        drop(original);
        let new = CancellationToken::new();
        let successor = registry.register("source-new", new.clone()).unwrap();
        assert!(!registry.cancel("source-old").unwrap());
        assert!(!new.is_cancelled());
        assert!(registry.cancel("source-new").unwrap());
        assert!(new.is_cancelled());
        drop(successor);
    }
}

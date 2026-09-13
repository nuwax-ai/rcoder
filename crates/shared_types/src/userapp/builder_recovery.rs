//! Host-side builder recovery, separate from address lookup and deletion.
#[async_trait::async_trait]
pub trait UserAppBuilderRecovery: Send + Sync {
    /// Resume only this unclaimed snapshot. Implementations must recheck its
    /// lifecycle, owner and revision before using the existing execution kernel.
    /// Return false when a lock or changed snapshot prevents claiming it.
    async fn resume_pending(
        &self,
        operation: &crate::UserAppOperationRecord,
    ) -> Result<bool, String>;
    /// Reconcile proven completion only; this must never invoke a Pending execution kernel.
    async fn reconcile_completed(
        &self,
        operation: &crate::UserAppOperationRecord,
    ) -> Result<bool, String>;
}

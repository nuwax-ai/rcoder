//! Read-only physical authority for an official proxy confirmation. Network
//! PID/hash/marker observations cannot replace a retained Child or engine claim.
use anyhow::{Context, Result};
use tokio::time::Instant;

use super::apply_status::ConfirmedPublication;

pub(crate) async fn verify_physical(
    expected: &ConfirmedPublication,
    deadline: Instant,
) -> Result<()> {
    tokio::time::timeout_at(deadline, async {
        match crate::supervisord_host::SupervisordHost::from_env().await? {
            Some(host) => host.verify_entry_confirmation(expected).await,
            None => {
                crate::supervisor::resident::verify_entry_confirmation(expected, deadline).await
            }
        }
    })
    .await
    .context("official proxy physical observation deadline exhausted")??;
    anyhow::ensure!(
        Instant::now() < deadline,
        "official proxy physical proof completed after its deadline"
    );
    Ok(())
}

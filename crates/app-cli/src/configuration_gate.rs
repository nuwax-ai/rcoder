//! Platform-managed cold starts expose management before starting business.
//! Activation is durable and bound to the immutable generation and config.
use anyhow::{Context, Result, ensure};
use shared_types::RuntimeConfigurationActivation;
use std::{io::Write, path::PathBuf};
use tokio_util::sync::CancellationToken;

pub(crate) struct ConfigurationGate {
    expected: RuntimeConfigurationActivation,
    path: PathBuf,
    handoff: Option<shared_types::RuntimeGenerationHandoff>,
    prepared: std::sync::OnceLock<shared_types::RuntimeGenerationPrepared>,
}

impl ConfigurationGate {
    pub fn from_env(root: PathBuf) -> Result<Option<Self>> {
        let version = match std::env::var(shared_types::APP_RUNTIME_CONFIGURATION_VERSION) {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(error) => return Err(error).context("read managed configuration version"),
        };
        let version = version
            .parse::<i64>()
            .context("invalid managed configuration version")?;
        ensure!(
            std::env::var("APP_CLI_DEPLOY_TOKEN")
                .ok()
                .is_some_and(|token| !token.trim().is_empty()),
            "managed configuration requires an enabled deployment management channel"
        );
        let expected = RuntimeConfigurationActivation {
            config_version: version,
            operation_id: std::env::var(shared_types::APP_DEPLOY_OPERATION_ID)
                .context("managed configuration requires deployment operation identity")?,
            deployment_generation: std::env::var(shared_types::APP_DEPLOY_GENERATION_ID)
                .context("managed configuration requires deployment generation")?,
        };
        let mut gate = Self::new(root, expected)?;
        gate.handoff = std::env::var(shared_types::APP_RUNTIME_GENERATION_HANDOFF)
            .map(Some)
            .or_else(|error| match error {
                std::env::VarError::NotPresent => Ok(None),
                other => Err(other),
            })?
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .context("decode generation handoff authorization")?;
        if let Some(handoff) = &gate.handoff {
            handoff.validate().map_err(anyhow::Error::msg)?;
            ensure!(
                std::env::var("APP_ID").ok().as_deref() == Some(handoff.app_id.as_str()),
                "handoff application identity mismatch"
            );
            ensure!(
                handoff.activation == gate.expected,
                "handoff activation identity mismatch"
            );
        }
        Ok(Some(gate))
    }

    fn new(root: PathBuf, expected: RuntimeConfigurationActivation) -> Result<Self> {
        ensure!(
            expected.config_version > 0
                && !expected.operation_id.trim().is_empty()
                && !expected.deployment_generation.trim().is_empty(),
            "incomplete managed configuration identity"
        );
        Ok(Self {
            expected,
            path: root.join("runtime-configuration-activation.json"),
            handoff: None,
            prepared: std::sync::OnceLock::new(),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_handoff(
        root: PathBuf,
        handoff: shared_types::RuntimeGenerationHandoff,
    ) -> Self {
        let mut gate = Self::new(root, handoff.activation.clone()).unwrap();
        gate.handoff = Some(handoff);
        gate
    }

    pub fn handoff(&self) -> Option<&shared_types::RuntimeGenerationHandoff> {
        self.handoff.as_ref()
    }

    pub fn publish_prepared(
        &self,
        prepared: shared_types::RuntimeGenerationPrepared,
    ) -> Result<()> {
        ensure!(
            self.handoff.as_ref() == Some(&prepared.authorization),
            "prepared handoff identity mismatch"
        );
        if let Some(existing) = self.prepared.get() {
            ensure!(existing == &prepared, "prepared handoff changed");
            return Ok(());
        }
        self.prepared
            .set(prepared)
            .map_err(|_| anyhow::anyhow!("prepared handoff already published"))
    }

    pub fn prepared(&self) -> Option<&shared_types::RuntimeGenerationPrepared> {
        self.prepared.get()
    }

    fn receipt(&self) -> Result<Option<RuntimeConfigurationActivation>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                ensure!(
                    bytes.len() <= 16 * 1024,
                    "configuration activation receipt exceeds size limit"
                );
                Ok(Some(
                    serde_json::from_slice(&bytes)
                        .context("invalid configuration activation receipt")?,
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("read configuration activation receipt"),
        }
    }

    pub fn matches(&self, receipt: &RuntimeConfigurationActivation) -> bool {
        *receipt == self.expected
    }

    pub fn activate(&self, receipt: &RuntimeConfigurationActivation) -> Result<()> {
        ensure!(
            *receipt == self.expected,
            "configuration activation identity does not match this generation"
        );
        ensure!(
            self.handoff.is_none() || self.prepared.get().is_some(),
            "generation handoff has not been prepared"
        );
        // Corrupt or unreadable prior state is not silently replaced.
        if self.receipt()?.as_ref() == Some(receipt) {
            return Ok(());
        }
        let root = self
            .path
            .parent()
            .context("configuration receipt has no state root")?;
        let mut pending = tempfile::NamedTempFile::new_in(root)?;
        pending.write_all(&serde_json::to_vec(receipt)?)?;
        pending.as_file().sync_all()?;
        pending
            .persist(&self.path)
            .map_err(|error| error.error)
            .context("commit configuration activation receipt")?;
        #[cfg(unix)]
        std::fs::File::open(root)?.sync_all()?;
        ensure!(
            self.receipt()?.as_ref() == Some(receipt),
            "configuration activation readback mismatch"
        );
        Ok(())
    }

    /// No database transaction is held across this wait. The runtime retains
    /// its exclusive ownership; management remains responsive, readiness stays
    /// false, and shutdown cancels the wait.
    pub async fn wait(&self, cancellation: &CancellationToken) -> Result<()> {
        loop {
            ensure!(
                !cancellation.is_cancelled(),
                "managed configuration startup cancelled"
            );
            if self.receipt()?.as_ref() == Some(&self.expected) {
                return Ok(());
            }
            tokio::select! {
                _ = cancellation.cancelled() => anyhow::bail!("managed configuration startup cancelled"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {},
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity(version: i64) -> RuntimeConfigurationActivation {
        RuntimeConfigurationActivation {
            operation_id: format!("operation{version}"),
            deployment_generation: format!("generation{version}"),
            config_version: version,
        }
    }
    #[tokio::test]
    async fn startup_requires_exact_receipt_and_survives_process_reopen() {
        let root = tempfile::tempdir().unwrap();
        let gate = ConfigurationGate::new(root.path().into(), identity(2)).unwrap();
        let cancellation = CancellationToken::new();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(5),
                gate.wait(&cancellation)
            )
            .await
            .is_err()
        );
        assert!(gate.activate(&identity(1)).is_err());
        assert!(!gate.path.exists());
        gate.activate(&identity(2)).unwrap();
        gate.activate(&identity(2)).unwrap();
        let reopened = ConfigurationGate::new(root.path().into(), identity(2)).unwrap();
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            reopened.wait(&cancellation),
        )
        .await
        .unwrap()
        .unwrap();
        let replacement = ConfigurationGate::new(root.path().into(), identity(3)).unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(5),
                replacement.wait(&cancellation)
            )
            .await
            .is_err()
        );
    }
    #[tokio::test]
    async fn corruption_and_cancellation_cannot_open_business_startup() {
        let root = tempfile::tempdir().unwrap();
        let gate = ConfigurationGate::new(root.path().into(), identity(1)).unwrap();
        std::fs::write(&gate.path, b"broken").unwrap();
        assert!(gate.wait(&CancellationToken::new()).await.is_err());
        assert!(gate.activate(&identity(1)).is_err());
        assert_eq!(std::fs::read(&gate.path).unwrap(), b"broken");
        std::fs::remove_file(&gate.path).unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(gate.wait(&cancellation).await.is_err());
        assert!(!gate.path.exists());
    }
}

//! Preserve ordinary historical metadata before an explicit Source replaces it.
use super::*;
use sha2::{Digest, Sha256};

impl Journal {
    /// Called while the journal lease is held. Identical bytes share one backup
    /// across retries; this diagnostic copy never grants execution ownership.
    pub(in crate::orchestration::server) fn preserve_source_history(&self) -> Result<PathBuf> {
        let bytes = std::fs::read(self.root.join(".deploy-operation.json"))
            .context("read historical deployment receipt before Source replacement")?;
        let backup = self.root.join(format!(
            ".deploy-operation.source-history-{}.json",
            hex::encode(Sha256::digest(&bytes))
        ));
        match std::fs::read(&backup) {
            Ok(saved) => anyhow::ensure!(saved == bytes, "historical receipt backup changed"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
                file.write_all(&bytes)?;
                file.as_file().sync_all()?;
                file.persist(&backup)
                    .map_err(|error| error.error)
                    .context("preserve historical receipt before Source replacement")?;
                #[cfg(unix)]
                File::open(&self.root)?.sync_all()?;
                anyhow::ensure!(
                    std::fs::read(&backup)? == bytes,
                    "historical receipt backup readback mismatch"
                );
            }
            Err(error) => return Err(error).context("inspect historical receipt backup"),
        }
        Ok(backup)
    }
}

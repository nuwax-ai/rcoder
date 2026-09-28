use super::*;

/// Stream ZIP entries without application-level capacity or entry-count quotas.
/// Paths, symbolic links and executable permissions retain their safety checks.
pub(crate) fn extract_zip_sync(zip_path: &Path, dest: &Path) -> Result<()> {
    use std::io::Read;
    let mut archive =
        zip::ZipArchive::new(std::fs::File::open(zip_path)?).context("open artifact zip")?;
    let mut links = shared_types::archive_links::ArchiveSymlinks::default();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let rel = entry
            .enclosed_name()
            .context("zip entry escapes destination (zip-slip)")?;
        let out_path = shared_types::archive_links::checked_archive_output(dest, &rel)?;
        if entry.is_symlink() {
            let mut target = String::new();
            (&mut entry)
                .take((shared_types::archive_links::MAX_ARCHIVE_LINK_BYTES + 1) as u64)
                .read_to_string(&mut target)?;
            links.record(&rel, &target)?;
            continue;
        }
        if entry.is_dir() {
            std::fs::create_dir_all(out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&out_path)?;
        std::io::copy(&mut entry, &mut out).context("extract artifact entry")?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(out_path, std::fs::Permissions::from_mode(mode & 0o777))?;
        }
    }
    links.install(dest)?;
    Ok(())
}

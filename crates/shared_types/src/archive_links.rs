//! Shared archive link contract: preserve internal dependency links without
//! allowing archive writes or link chains to escape the extraction root.
use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};

#[cfg(unix)]
use std::collections::VecDeque;

pub const MAX_ARCHIVE_LINK_BYTES: usize = 4096;

/// Validate a relative archive path and refuse writes through existing links.
/// The caller owns/trusts `root`; links created by this archive are deferred.
pub fn checked_archive_output(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut output = root.to_path_buf();
    let mut normal = false;
    for part in relative.components() {
        match part {
            Component::CurDir => continue,
            Component::Normal(name) => {
                output.push(name);
                normal = true;
            }
            _ => bail!("archive entry escapes extraction root"),
        }
        match std::fs::symlink_metadata(&output) {
            Ok(metadata) if metadata.is_symlink() => {
                bail!("archive entry would write through a symbolic link")
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect archive output"),
        }
    }
    if !normal {
        bail!("archive entry has an empty path");
    }
    Ok(output)
}

/// Resolve links component by component without following an unchecked prefix.
/// Missing optional dependencies are allowed; escaping the root never is.
#[cfg(unix)]
fn resolve_filesystem_link(root: &Path, relative: &Path) -> Result<()> {
    let mut pending: VecDeque<_> = relative
        .components()
        .map(|c| c.as_os_str().to_os_string())
        .collect();
    let mut resolved = PathBuf::new();
    let mut hops = 0;
    while let Some(part) = pending.pop_front() {
        if part == "." {
            continue;
        }
        if part == ".." {
            if !resolved.pop() {
                bail!("installed archive link escapes root");
            }
            continue;
        }
        resolved.push(part);
        match std::fs::symlink_metadata(root.join(&resolved)) {
            Ok(metadata) if metadata.is_symlink() => {
                hops += 1;
                if hops > 40 {
                    bail!("installed archive link cycle or excessive depth");
                }
                let target = std::fs::read_link(root.join(&resolved))?;
                if target.is_absolute() {
                    bail!("installed archive link has absolute target");
                }
                resolved.pop();
                for component in target.components().rev() {
                    pending.push_front(component.as_os_str().to_os_string());
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect installed archive link"),
        }
    }
    Ok(())
}

#[derive(Default)]
pub struct ArchiveSymlinks {
    links: BTreeMap<PathBuf, PathBuf>,
    case_keys: BTreeMap<String, PathBuf>,
}

impl ArchiveSymlinks {
    pub fn record(&mut self, name: &Path, target: &str) -> Result<()> {
        if target.is_empty()
            || target.len() > MAX_ARCHIVE_LINK_BYTES
            || target.contains(['\0', '\\'])
        {
            bail!("invalid archive symbolic link target");
        }
        let target = Path::new(target);
        if target.is_absolute()
            || target
                .components()
                .any(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
        {
            bail!("absolute archive symbolic link target is not portable");
        }
        let mut normalized = PathBuf::new();
        for part in name.components() {
            match part {
                Component::Normal(part) => normalized.push(part),
                Component::CurDir => {}
                _ => bail!("archive symbolic link path escapes root"),
            }
        }
        if normalized.as_os_str().is_empty() || self.links.contains_key(&normalized) {
            bail!("empty or duplicate archive symbolic link path");
        }
        let key = normalized.to_string_lossy().to_lowercase();
        if self.case_keys.contains_key(&key) {
            bail!("case-ambiguous archive symbolic links");
        }
        self.case_keys.insert(key, normalized.clone());
        self.links.insert(normalized, target.to_path_buf());
        Ok(())
    }

    #[cfg(unix)]
    fn resolve(&self, name: &Path, target: &Path) -> Result<PathBuf> {
        let parent = name
            .parent()
            .context("archive symbolic link has no parent")?;
        let combined = parent.join(target);
        let mut pending: VecDeque<_> = combined
            .components()
            .map(|c| c.as_os_str().to_os_string())
            .collect();
        let mut resolved = PathBuf::new();
        let mut hops = 0;
        while let Some(part) = pending.pop_front() {
            if part == "." {
                continue;
            }
            if part == ".." {
                if !resolved.pop() {
                    bail!("archive symbolic link chain escapes root");
                }
                continue;
            }
            resolved.push(part);
            if let Some(original) = self
                .case_keys
                .get(&resolved.to_string_lossy().to_lowercase())
                && original != &resolved
            {
                bail!("case-ambiguous archive symbolic link target");
            }
            if let Some(next) = self.links.get(&resolved) {
                hops += 1;
                if hops > 40 {
                    bail!("archive symbolic link cycle or excessive depth");
                }
                resolved.pop();
                for part in next.components().rev() {
                    pending.push_front(part.as_os_str().to_os_string());
                }
            }
        }
        Ok(resolved)
    }

    /// Validate the graph, install links, then verify filesystem resolution.
    /// Optional dangling dependencies remain supported within the artifact root.
    pub fn install(self, root: &Path) -> Result<()> {
        if self.links.is_empty() {
            return Ok(());
        }
        #[cfg(not(unix))]
        bail!("archive symbolic links require a Unix deployment runtime");
        #[cfg(unix)]
        {
            let root = root
                .canonicalize()
                .context("resolve archive extraction root")?;
            for (name, target) in &self.links {
                if name.ancestors().skip(1).any(|p| self.links.contains_key(p)) {
                    bail!("archive entry descends through a symbolic link");
                }
                let output = checked_archive_output(&root, name)?;
                if output.try_exists()? {
                    bail!("archive symbolic link conflicts with an existing entry");
                }
                let resolved = self.resolve(name, target)?;
                if !resolved.as_os_str().is_empty() {
                    checked_archive_output(&root, &resolved)?;
                }
            }
            let mut created = Vec::new();
            let result = (|| -> Result<()> {
                for (name, target) in &self.links {
                    let output = root.join(name);
                    std::fs::create_dir_all(
                        output.parent().context("archive link has no parent")?,
                    )?;
                    std::os::unix::fs::symlink(target, &output)
                        .context("create archive symbolic link")?;
                    created.push(output);
                }
                // Check actual filesystem aliases too, before callers expose the
                // operation-owned preparation directory to any application.
                for name in self.links.keys() {
                    resolve_filesystem_link(&root, name)?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                let mut cleanup_errors = Vec::new();
                for path in created.iter().rev() {
                    if let Err(cleanup) = std::fs::remove_file(path) {
                        cleanup_errors.push(cleanup.to_string());
                    }
                }
                if !cleanup_errors.is_empty() {
                    bail!(
                        "archive link validation: {error:#}; cleanup: {}",
                        cleanup_errors.join("; ")
                    );
                }
                return Err(error);
            }
            Ok(())
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn internal_forward_links_preserve_dependency_resolution() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("lib")).unwrap();
        std::fs::write(root.path().join("lib/index.js"), "module").unwrap();
        let mut links = ArchiveSymlinks::default();
        links
            .record(Path::new("modules/package"), "../alias")
            .unwrap();
        links.record(Path::new("alias"), "lib").unwrap();
        links.install(root.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("modules/package/index.js")).unwrap(),
            "module"
        );
    }
    #[test]
    fn traversal_cycles_and_existing_links_are_rejected() {
        for pairs in [
            vec![("bad", "../outside")],
            vec![("a", "b"), ("b", "a")],
            vec![("a", "."), ("b", "a/../outside")],
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut links = ArchiveSymlinks::default();
            for (name, target) in pairs {
                links.record(Path::new(name), target).unwrap();
            }
            assert!(links.install(root.path()).is_err());
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        }
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        assert!(checked_archive_output(root.path(), Path::new("escape/file")).is_err());
        let mut links = ArchiveSymlinks::default();
        assert!(links.record(Path::new("bad"), "/etc/passwd").is_err());
    }
    #[test]
    fn optional_dangling_dependency_stays_inside_root() {
        let root = tempfile::tempdir().unwrap();
        let mut links = ArchiveSymlinks::default();
        links
            .record(
                Path::new("node_modules/optional"),
                ".pnpm/optional/index.js",
            )
            .unwrap();
        links.install(root.path()).unwrap();
        assert!(root.path().join("node_modules/optional").is_symlink());
        assert!(!root.path().join("node_modules/optional").exists());
    }
    #[test]
    fn filesystem_aliases_cannot_hide_an_escape() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("é"), "probe").unwrap();
        let aliases = root.path().join("e\u{301}").exists();
        std::fs::remove_file(root.path().join("é")).unwrap();
        let mut links = ArchiveSymlinks::default();
        links.record(Path::new("é"), ".").unwrap();
        links.record(Path::new("b"), "e\u{301}/../outside").unwrap();
        let result = links.install(root.path());
        if aliases {
            assert!(result.is_err());
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        } else {
            assert!(result.is_ok());
            assert!(root.path().join("b").is_symlink());
        }
    }
}

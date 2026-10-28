//! Official Pingap interprets `-c` as a glob. Windows canonical paths contain
//! a verbatim `?` prefix, so pass an equivalent ordinary spelling to that tool.
//! Owner identity and immutable snapshot paths remain canonical internally.
use std::path::{Path, PathBuf};

use anyhow::Result;

pub(crate) fn external_config_path(path: &Path) -> Result<PathBuf> {
    #[cfg(windows)]
    {
        use anyhow::{Context, ensure};

        let Some(argument) = ordinary_windows_path(path) else {
            return Ok(path.to_owned());
        };
        // Converting reserved Windows names or trailing-dot components can
        // change which file is opened. Confirm the current file, never infer
        // equivalence from a string prefix or change the stored identity.
        let original = std::fs::canonicalize(path)
            .context("resolve original official proxy configuration path")?;
        let ordinary = std::fs::canonicalize(&argument)
            .context("resolve external official proxy configuration argument")?;
        ensure!(
            original == ordinary,
            "external official proxy configuration argument resolves to a different file"
        );
        Ok(argument)
    }
    #[cfg(not(windows))]
    {
        Ok(path.to_owned())
    }
}

#[cfg(windows)]
fn ordinary_windows_path(path: &Path) -> Option<PathBuf> {
    use std::{ffi::OsString, path::Component, path::Prefix};

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return None;
    };
    let mut ordinary = match prefix.kind() {
        Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:", char::from(drive))),
        Prefix::VerbatimUNC(server, share) => {
            let mut root = OsString::from(r"\\");
            root.push(server);
            root.push(r"\");
            root.push(share);
            PathBuf::from(root)
        }
        Prefix::Verbatim(_) | Prefix::DeviceNS(_) | Prefix::UNC(_, _) | Prefix::Disk(_) => {
            return None;
        }
    };
    for component in components {
        ordinary.push(component.as_os_str());
    }
    Some(ordinary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_config_argument_has_no_new_existence_requirement() {
        let path = Path::new("missing directory/配置 file.toml");
        assert_eq!(external_config_path(path).unwrap(), path);
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_keeps_literal_glob_characters_unchanged() {
        let path = Path::new("/missing/with?literal/file.toml");
        assert_eq!(external_config_path(path).unwrap(), path);
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_and_unc_spellings_keep_unicode_and_spaces() {
        for (canonical, ordinary) in [
            (
                r"\\?\C:\目录 space\pingap.toml",
                r"C:\目录 space\pingap.toml",
            ),
            (
                r"\\?\UNC\server\share\目录 space\pingap.toml",
                r"\\server\share\目录 space\pingap.toml",
            ),
        ] {
            assert_eq!(
                ordinary_windows_path(Path::new(canonical)),
                Some(PathBuf::from(ordinary))
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_external_argument_keeps_the_actual_file_and_long_paths() {
        let root = tempfile::tempdir().unwrap();
        let mut folder = root.path().join("目录 space");
        for _ in 0..5 {
            folder = folder.join("long-component-abcdefghijklmnopqrstuvwxyz-0123456789");
        }
        std::fs::create_dir_all(&folder).unwrap();
        let config = folder.join("pingap.toml");
        std::fs::write(&config, b"[basic]\nthreads=1\n").unwrap();
        let canonical = std::fs::canonicalize(&config).unwrap();
        let argument = external_config_path(&canonical).unwrap();
        assert!(!argument.as_os_str().to_string_lossy().starts_with(r"\\?\"));
        assert!(argument.as_os_str().len() > 260);
        assert_eq!(std::fs::canonicalize(&argument).unwrap(), canonical);
        assert_eq!(std::fs::read(argument).unwrap(), b"[basic]\nthreads=1\n");
    }

    #[cfg(windows)]
    #[test]
    fn windows_failed_observation_is_not_hidden_and_can_be_retried() {
        let root = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap().join("new.toml");
        assert!(external_config_path(&canonical).is_err());
        std::fs::write(&canonical, b"[basic]\n").unwrap();
        assert_eq!(
            std::fs::read(external_config_path(&canonical).unwrap()).unwrap(),
            b"[basic]\n"
        );
    }
}

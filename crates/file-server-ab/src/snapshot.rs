//! 工作区终态快照对比（目录树、git 状态、生成物归一化）。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result};

use crate::compare::*;
use crate::report::*;
use crate::types::*;
use crate::util::*;

pub(crate) fn compare_snapshots(
    rust: &[SnapshotEntry],
    ts: &[SnapshotEntry],
) -> Result<Vec<Difference>> {
    let rust_by_path: BTreeMap<_, _> = rust
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let ts_by_path: BTreeMap<_, _> = ts
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let paths: BTreeSet<_> = rust_by_path
        .keys()
        .chain(ts_by_path.keys())
        .copied()
        .collect();
    let mut differences = Vec::new();
    for path in paths {
        let rust_entry = rust_by_path.get(path).copied();
        let ts_entry = ts_by_path.get(path).copied();
        if rust_entry != ts_entry {
            differences.push(diff(
                "workspace-state",
                &format!("/{path}"),
                if rust_entry.is_some() && ts_entry.is_some() {
                    "value"
                } else if rust_entry.is_some() {
                    "rust_only"
                } else {
                    "ts_only"
                },
                rust_entry
                    .map(|entry| {
                        serde_json::to_value(entry).context("serialize Rust snapshot entry")
                    })
                    .transpose()?,
                ts_entry
                    .map(|entry| serde_json::to_value(entry).context("serialize TS snapshot entry"))
                    .transpose()?,
            ));
        }
    }
    Ok(differences)
}

pub(crate) fn snapshot_roots(root: &Path) -> Result<Vec<SnapshotEntry>> {
    let mut entries = Vec::new();
    for name in [
        "project-workspace",
        "computer-workspace",
        "project-zips",
        "project-nginx",
    ] {
        let path = root.join(name);
        if path.exists() {
            snapshot_dir(root, &path, &mut entries)?;
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

pub(crate) fn snapshot_git_state(repo_root: &Path) -> Result<GitStateSnapshot> {
    let repository_exists = repo_root.join(".git").exists();
    let mut snapshot = GitStateSnapshot {
        repository_exists,
        head_reference: None,
        head_tree: None,
        refs: BTreeMap::new(),
        index_entries: Vec::new(),
        status_entries: Vec::new(),
        capture_errors: Vec::new(),
    };
    if !repository_exists {
        snapshot
            .capture_errors
            .push("Git worktree has no .git entry".to_string());
        return Ok(snapshot);
    }

    if let Some(bytes) = capture_git_command(
        repo_root,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        &mut snapshot.capture_errors,
    ) {
        snapshot.head_reference = String::from_utf8_lossy(&bytes).trim().to_string().into();
    }
    if let Some(bytes) = capture_git_command(
        repo_root,
        &["rev-parse", "--verify", "HEAD^{tree}"],
        &mut snapshot.capture_errors,
    ) {
        snapshot.head_tree = String::from_utf8_lossy(&bytes).trim().to_string().into();
    }
    if let Some(bytes) = capture_git_command(
        repo_root,
        &["for-each-ref", "--format=%(refname)"],
        &mut snapshot.capture_errors,
    ) {
        for reference in String::from_utf8_lossy(&bytes).lines() {
            if reference.is_empty() {
                continue;
            }
            let revision = format!("{reference}^{{tree}}");
            let args = ["rev-parse", "--verify", revision.as_str()];
            if let Some(tree) = capture_git_command(repo_root, &args, &mut snapshot.capture_errors)
            {
                snapshot.refs.insert(
                    reference.to_string(),
                    String::from_utf8_lossy(&tree).trim().to_string(),
                );
            }
        }
    }
    if let Some(bytes) = capture_git_command(
        repo_root,
        &["ls-files", "--stage", "-z"],
        &mut snapshot.capture_errors,
    ) {
        snapshot.index_entries = git_index_records(&bytes);
    }
    if let Some(bytes) = capture_git_command(
        repo_root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        &mut snapshot.capture_errors,
    ) {
        snapshot.status_entries = nul_records(&bytes);
        snapshot.status_entries.sort();
    }
    Ok(snapshot)
}

pub(crate) fn capture_git_command(
    repo_root: &Path,
    args: &[&str],
    errors: &mut Vec<String>,
) -> Option<Vec<u8>> {
    let output = match ProcessCommand::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            errors.push(format!("git {} could not start: {error}", args.join(" ")));
            return None;
        }
    };
    if output.status.success() {
        Some(output.stdout)
    } else {
        errors.push(format!(
            "git {} exited with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
        None
    }
}

pub(crate) fn nul_records(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| String::from_utf8_lossy(record).into_owned())
        .collect()
}

pub(crate) fn git_index_records(bytes: &[u8]) -> Vec<String> {
    let mut entries = bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(
            |record| match record.iter().position(|byte| *byte == b'\t') {
                Some(separator) => format!(
                    "{}\t{}",
                    String::from_utf8_lossy(&record[separator + 1..]),
                    String::from_utf8_lossy(&record[..separator])
                ),
                None => String::from_utf8_lossy(record).into_owned(),
            },
        )
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

/// Matches the `<epoch-millis>\n` shape both implementations write into
/// `.dynamic_add.lock`.
pub(crate) fn is_timestamp_marker(bytes: &[u8]) -> bool {
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    !body.is_empty() && body.iter().all(u8::is_ascii_digit)
}

/// Both implementations stamp generated `.npmrc` files with a local-time comment
/// (`# 自动生成于 YYYY-MM-DD HH:MM:SS`). Independent project creation can straddle a
/// second boundary, so normalize that one line before hashing; any other content
/// difference still compares byte-wise.
pub(crate) fn normalize_generated_npmrc(contents: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(contents).ok()?;
    let mut normalized = String::with_capacity(text.len());
    let mut changed = false;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches('\n');
        if let Some(stamp) = body.strip_prefix("# 自动生成于 ") {
            let digits_or_sep = |(index, byte): (usize, u8)| match index {
                4 | 7 => byte == b'-',
                10 => byte == b' ',
                13 | 16 => byte == b':',
                _ => byte.is_ascii_digit(),
            };
            if stamp.len() == 19 && stamp.bytes().enumerate().all(digits_or_sep) {
                normalized.push_str("# 自动生成于 <timestamp>\n");
                changed = true;
                continue;
            }
        }
        normalized.push_str(line);
    }
    changed.then_some(normalized)
}

pub(crate) fn snapshot_dir(
    root: &Path,
    dir: &Path,
    entries: &mut Vec<SnapshotEntry>,
) -> Result<()> {
    let mut children = fs::read_dir(dir)
        .with_context(|| format!("read workspace directory {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for entry in children {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if [".git", "node_modules", "cache", "logs", "tmp"].contains(&name.as_ref()) {
            continue;
        }
        let path = entry.path();
        let relative = path
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        let metadata = fs::symlink_metadata(&path)?;
        let mode = mode_string(&metadata);
        if metadata.file_type().is_symlink() {
            entries.push(SnapshotEntry {
                path: relative,
                kind: "symlink".into(),
                sha256: None,
                size_bytes: None,
                mode,
                symlink_target: Some(fs::read_link(&path)?.to_string_lossy().into_owned()),
            });
        } else if metadata.is_dir() {
            entries.push(SnapshotEntry {
                path: relative,
                kind: "directory".into(),
                sha256: None,
                size_bytes: None,
                mode,
                symlink_target: None,
            });
            snapshot_dir(root, &path, entries)?;
        } else if metadata.is_file() {
            let contents = fs::read(&path)?;
            // Two state-snapshot normalizations, both shape-preserving:
            // - `.dynamic_add.lock` marks dynamically installed skills; both
            //   implementations write "<epoch-millis>\n", so the value differs only by
            //   installation time. Compare its shape, not the timestamp.
            // - ZIP artifacts embed entry mtimes and compression details, so raw bytes
            //   differ between equivalent archives. Apply the same entry-semantics
            //   comparison used for ZIP HTTP responses; unparseable zips still compare
            //   byte-wise.
            // Existence, kind, and mode are always compared.
            let (sha256, size_bytes) =
                if name == ".dynamic_add.lock" && is_timestamp_marker(&contents) {
                    (Some("<dynamic-timestamp>".to_string()), None)
                } else if name == ".npmrc" {
                    match normalize_generated_npmrc(&contents) {
                        Some(normalized) => (
                            Some(sha256(normalized.as_bytes())),
                            Some(contents.len() as u64),
                        ),
                        None => (Some(sha256(&contents)), Some(contents.len() as u64)),
                    }
                } else if path.extension().is_some_and(|extension| extension == "zip") {
                    match zip_semantic_entries(&contents) {
                        Ok(entries) => {
                            let canonical = serde_json::to_string(&entries)
                                .context("serialize ZIP semantic entries")?;
                            (
                                Some(format!("semantic-zip:{}", sha256(canonical.as_bytes()))),
                                None,
                            )
                        }
                        Err(_) => (Some(sha256(&contents)), Some(contents.len() as u64)),
                    }
                } else {
                    (Some(sha256(&contents)), Some(contents.len() as u64))
                };
            entries.push(SnapshotEntry {
                path: relative,
                kind: "file".into(),
                sha256,
                size_bytes,
                mode,
                symlink_target: None,
            });
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn mode_string(metadata: &fs::Metadata) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    Some(format!("{:04o}", metadata.permissions().mode() & 0o7777))
}

#[cfg(not(unix))]
pub(crate) fn mode_string(_metadata: &fs::Metadata) -> Option<String> {
    None
}

//! Read Pingap sources while preserving safe file locations and native I/O errors.
//! The pinned Pingap loader's format preference and glob order remain unchanged.
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

use pingap_config::PingapConfig;
use workspace_manifest::{DiagnosticKind, ValidationIssue};

#[derive(Debug)]
pub(crate) struct ConfigSourceError {
    pub kind: DiagnosticKind,
    pub issue: Box<ValidationIssue>,
    source: Option<io::Error>,
}

impl std::fmt::Display for ConfigSourceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.issue, formatter)
    }
}

impl std::error::Error for ConfigSourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|source| source as _)
    }
}

impl ConfigSourceError {
    fn io(file: &Path, operation: &str, source: io::Error) -> Self {
        Self {
            kind: DiagnosticKind::Io,
            issue: Box::new(
                ValidationIssue::new(format!("{operation}: {source}"))
                    .at_file(file.display().to_string())
                    .at_field("pingap.config")
                    .with_hint(
                        "check the source path, file type, and read permissions, then re-run",
                    ),
            ),
            source: Some(source),
        }
    }

    fn parse(file: &Path, text_and_offset: Option<(&str, usize)>) -> Self {
        let mut message = "invalid Pingap source syntax, field, or value".to_owned();
        if let Some((text, offset)) = text_and_offset {
            let (mut line, mut column) = (1, 1);
            for (_, character) in text.char_indices().take_while(|(index, _)| *index < offset) {
                if character == '\n' {
                    line += 1;
                    column = 1;
                } else {
                    column += 1;
                }
            }
            message.push_str(&format!(" at line {line}, column {column}"));
        }
        Self {
            kind: DiagnosticKind::Parse,
            issue: Box::new(ValidationIssue::new(message)
                .at_file(file.display().to_string())
                .at_field("pingap.config")
                .with_hint("check the Pingap fields and source syntax at the reported location; configuration values are omitted")),
            source: None,
        }
    }
}

struct SourceChunk {
    // Offsets in the lossy UTF-8 document parsed by PingapConfig::new.
    range: Range<usize>,
    file: PathBuf,
    original_text: Option<String>,
}

pub(crate) struct ConfigSource {
    pub bytes: Vec<u8>,
    file: PathBuf,
    chunks: Vec<SourceChunk>,
}

impl ConfigSource {
    pub fn parse(self) -> Result<PingapConfig, ConfigSourceError> {
        PingapConfig::new(&self.bytes, true).map_err(|error| {
            let span = match error {
                pingap_config::Error::De { source } => source.span(),
                _ => None,
            };
            let chunk = span
                .as_ref()
                .and_then(|span| {
                    self.chunks.iter().find(|chunk| {
                        chunk.range.start <= span.start && span.start <= chunk.range.end
                    })
                })
                .or_else(|| self.chunks.first().filter(|_| self.chunks.len() == 1));
            let location = chunk.and_then(|chunk| {
                chunk
                    .original_text
                    .as_deref()
                    .zip(span.as_ref())
                    .map(|(text, span)| (text, span.start.saturating_sub(chunk.range.start)))
            });
            ConfigSourceError::parse(
                chunk.map_or(self.file.as_path(), |chunk| &chunk.file),
                location,
            )
        })
    }
}

/// Read once, then parse those same bytes; no permission preflight or error-text matching.
pub(crate) async fn read_config(
    path: &Path,
    display_path: &Path,
) -> Result<ConfigSource, ConfigSourceError> {
    let metadata = tokio::fs::metadata(path).await.map_err(|error| {
        ConfigSourceError::io(display_path, "cannot observe Pingap source", error)
    })?;
    if !metadata.is_dir() {
        let bytes = tokio::fs::read(path).await.map_err(|error| {
            ConfigSourceError::io(display_path, "cannot read Pingap source", error)
        })?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        return Ok(ConfigSource {
            bytes,
            file: display_path.to_path_buf(),
            chunks: vec![SourceChunk {
                range: 0..text.len(),
                file: display_path.to_path_buf(),
                original_text: Some(text),
            }],
        });
    }
    type Convert = fn(&str) -> Result<String, pingap_config::Error>;
    let formats: [(&str, Option<Convert>); 3] = [
        ("toml", None),
        ("hcl", Some(pingap_config::hcl::convert_hcl_to_toml)),
        ("kdl", Some(pingap_config::kdl::convert_kdl_to_toml)),
    ];
    for (extension, convert) in formats {
        let pattern = format!("{}/**/*.{extension}", path.to_string_lossy());
        let paths = glob::glob(&pattern).map_err(|error| {
            ConfigSourceError::io(
                display_path,
                "cannot enumerate Pingap sources",
                io::Error::other(error),
            )
        })?;
        let files = paths.collect::<Result<Vec<_>, _>>().map_err(|error| {
            let file = reported_file(path, display_path, error.path());
            ConfigSourceError::io(&file, "cannot enumerate Pingap source", error.into())
        })?;
        if files.is_empty() {
            continue;
        }
        let mut bytes = Vec::new();
        let mut chunks = Vec::new();
        let mut offset = 0;
        for file in files {
            let display_file = reported_file(path, display_path, &file);
            let buffer = tokio::fs::read(&file).await.map_err(|error| {
                ConfigSourceError::io(&display_file, "cannot read Pingap source", error)
            })?;
            let text = String::from_utf8_lossy(&buffer);
            let (length, original_text) = match convert {
                None => {
                    toml::from_str::<toml::Value>(&text).map_err(|error| {
                        ConfigSourceError::parse(
                            &display_file,
                            error.span().map(|span| (text.as_ref(), span.start)),
                        )
                    })?;
                    bytes.extend_from_slice(&buffer);
                    (text.len(), Some(text.into_owned()))
                }
                Some(convert) => {
                    let converted = convert(&text)
                        .map_err(|_| ConfigSourceError::parse(&display_file, None))?;
                    bytes.extend_from_slice(converted.as_bytes());
                    (converted.len(), None)
                }
            };
            chunks.push(SourceChunk {
                range: offset..offset + length,
                file: display_file,
                original_text,
            });
            bytes.push(b'\n');
            offset += length + 1;
        }
        return Ok(ConfigSource {
            bytes,
            file: display_path.to_path_buf(),
            chunks,
        });
    }
    Ok(ConfigSource {
        bytes: Vec::new(),
        file: display_path.to_path_buf(),
        chunks: Vec::new(),
    })
}

fn reported_file(root: &Path, display_root: &Path, file: &Path) -> PathBuf {
    file.strip_prefix(root).map_or_else(
        |_| file.to_path_buf(),
        |relative| display_root.join(relative),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn directory_source_read_failure_preserves_native_io_and_file() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let directory = tempfile::tempdir().unwrap();
        assert_ne!(
            std::fs::metadata(directory.path()).unwrap().uid(),
            0,
            "permission regression requires a non-root Unix user"
        );
        let file = directory.path().join("00-unreadable.toml");
        std::fs::write(&file, "[basic]\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = read_config(directory.path(), Path::new("pingap")).await;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = result.err().expect("source read must fail");
        assert_eq!(error.kind, DiagnosticKind::Io);
        assert_eq!(
            error.issue.file.as_deref(),
            Some("pingap/00-unreadable.toml")
        );
        assert_eq!(
            error.source.as_ref().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[tokio::test]
    async fn directory_parse_failure_keeps_bad_file_position_without_source_values() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("00-valid.toml"),
            "[servers.main]\naddr = '0.0.0.0:9080'\n",
        )
        .unwrap();
        std::fs::write(directory.path().join("ignored.hcl"), "not valid HCL").unwrap();
        let source = read_config(directory.path(), Path::new("pingap"))
            .await
            .unwrap();
        let pinned_bytes =
            pingap_config::read_all_config_files(&directory.path().to_string_lossy())
                .await
                .unwrap();
        assert_eq!(
            source.bytes, pinned_bytes,
            "TOML wins over lower-priority formats with identical source bytes"
        );
        std::fs::write(
            directory.path().join("10-bad.toml"),
            "[upstreams.external]\naddrs = ['DO_NOT_DISCLOSE_SECRET', ???]\n",
        )
        .unwrap();
        let error = read_config(directory.path(), Path::new("pingap"))
            .await
            .err()
            .expect("syntax must fail");
        assert_eq!(error.kind, DiagnosticKind::Parse);
        assert_eq!(error.issue.file.as_deref(), Some("pingap/10-bad.toml"));
        let rendered = error.to_string();
        assert!(rendered.contains("line 2, column"));
        assert!(!rendered.contains("DO_NOT_DISCLOSE_SECRET"));
        assert!(!rendered.contains("addrs ="));
        // A cross-file TOML error is mapped back from concatenation to its file.
        std::fs::write(
            directory.path().join("10-bad.toml"),
            "[servers.main]\naddr = 'DO_NOT_DISCLOSE_SECRET'\n",
        )
        .unwrap();
        let source = read_config(directory.path(), Path::new("pingap"))
            .await
            .unwrap();
        let error = source.parse().unwrap_err();
        assert_eq!(error.issue.file.as_deref(), Some("pingap/10-bad.toml"));
        assert!(error.issue.message.contains("line 1, column"));
        assert!(!error.to_string().contains("DO_NOT_DISCLOSE_SECRET"));
    }
}

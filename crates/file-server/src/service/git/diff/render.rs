use gix::Repository;
use gix::diff::blob::sources;
use gix::diff::blob::unified_diff::{ConsumeHunk, ContextSize, DiffLineKind, HunkHeader};
use gix::diff::blob::{Algorithm, InternedInput, UnifiedDiff, diff_with_slider_heuristics};

use crate::error::{AppError, AppResult};

use super::types::{DiffResult, FileChange, FileSummary, Side};
use super::{ensure_output_size, is_binary, short_hash};

/// 渲染所有变更 → 完整 unified diff 文本 + summary。
pub(super) fn render_changes(
    repo: &Repository,
    changes: Vec<FileChange>,
    max_output_bytes: u64,
) -> AppResult<DiffResult> {
    let mut diff = String::new();
    let mut files = Vec::new();
    let mut total_ins = 0usize;
    let mut total_del = 0usize;
    for ch in changes {
        let old_bytes = ch.old.bytes.as_deref();
        let new_bytes = ch.new.bytes.as_deref();
        let content_changed = old_bytes != new_bytes;
        let mode_changed = ch.old.mode != ch.new.mode;
        if !content_changed && !mode_changed {
            continue;
        }

        let rendered = render_blob_diff(old_bytes, new_bytes)?;
        let old_hash = match old_bytes {
            Some(bytes) => short_hash(repo, bytes)?,
            None => "0000000".to_string(),
        };
        let new_hash = match new_bytes {
            Some(bytes) => short_hash(repo, bytes)?,
            None => "0000000".to_string(),
        };
        let header = assemble_header(
            &ch.path,
            &ch.old,
            &ch.new,
            &old_hash,
            &new_hash,
            content_changed,
            content_changed && !rendered.binary,
        )?;
        diff.push_str(&header);
        if rendered.binary && content_changed {
            let old_path = if ch.old.is_present() {
                format!("a/{}", ch.path)
            } else {
                "/dev/null".to_string()
            };
            let new_path = if ch.new.is_present() {
                format!("b/{}", ch.path)
            } else {
                "/dev/null".to_string()
            };
            diff.push_str(&format!("Binary files {old_path} and {new_path} differ\n"));
        } else if content_changed {
            diff.push_str(&rendered.hunks);
            if !rendered.hunks.is_empty() && !rendered.hunks.ends_with('\n') {
                diff.push('\n');
            }
        }
        files.push(FileSummary {
            file: ch.path,
            changes: rendered.insertions + rendered.deletions,
            insertions: rendered.insertions,
            deletions: rendered.deletions,
            binary: rendered.binary,
        });
        total_ins += rendered.insertions;
        total_del += rendered.deletions;
        ensure_output_size(&diff, max_output_bytes)?;
    }
    Ok(DiffResult {
        diff,
        files,
        insertions: total_ins,
        deletions: total_del,
    })
}

pub(super) struct Rendered {
    pub(super) hunks: String,
    pub(super) insertions: usize,
    pub(super) deletions: usize,
    binary: bool,
}

/// 渲染单文件 blob diff (对齐 nuwax makeDiffPatch 的 hunk 部分)。
pub(super) fn render_blob_diff(old: Option<&[u8]>, new: Option<&[u8]>) -> AppResult<Rendered> {
    let ob = old.unwrap_or(&[]);
    let nb = new.unwrap_or(&[]);
    if is_binary(ob) || is_binary(nb) {
        return Ok(Rendered {
            hunks: String::new(),
            insertions: 0,
            deletions: 0,
            binary: true,
        });
    }
    if ob == nb {
        return Ok(Rendered {
            hunks: String::new(),
            insertions: 0,
            deletions: 0,
            binary: false,
        });
    }
    let a = sources::byte_lines(ob);
    let b = sources::byte_lines(nb);
    let input = InternedInput::new(a, b);
    let diff = diff_with_slider_heuristics(Algorithm::Myers, &input);
    let output = UnifiedDiff::new(
        &diff,
        &input,
        HunkWriter::default(),
        ContextSize::symmetrical(3),
    )
    .consume()
    .map_err(|error| AppError::system(format!("git unified diff render: {error}")))?;
    Ok(Rendered {
        hunks: output.hunks,
        insertions: output.insertions,
        deletions: output.deletions,
        binary: false,
    })
}

#[derive(Default)]
struct HunkWriter {
    hunks: String,
    insertions: usize,
    deletions: usize,
}

impl ConsumeHunk for HunkWriter {
    type Out = Self;

    fn consume_hunk(
        &mut self,
        header: HunkHeader,
        lines: &[(DiffLineKind, &[u8])],
    ) -> std::io::Result<()> {
        use std::fmt::Write as _;

        writeln!(self.hunks, "{header}").map_err(std::io::Error::other)?;
        for &(kind, content) in lines {
            let prefix = match kind {
                DiffLineKind::Context => ' ',
                DiffLineKind::Add => {
                    self.insertions += 1;
                    '+'
                }
                DiffLineKind::Remove => {
                    self.deletions += 1;
                    '-'
                }
            };
            self.hunks.push(prefix);
            self.hunks
                .push_str(std::str::from_utf8(content).map_err(std::io::Error::other)?);
            if !content.ends_with(b"\n") {
                self.hunks.push('\n');
                self.hunks.push_str(r"\ No newline at end of file");
                self.hunks.push('\n');
            }
        }
        Ok(())
    }

    fn finish(self) -> Self::Out {
        self
    }
}

/// 拼装 git CLI 风格文件头 (对齐 nuwax makeDiffPatch part d)。
pub(super) fn assemble_header(
    path: &str,
    old: &Side,
    new: &Side,
    old_hash: &str,
    new_hash: &str,
    content_changed: bool,
    include_file_markers: bool,
) -> AppResult<String> {
    let has_old = old.is_present();
    let has_new = new.is_present();
    let mut header = format!("diff --git a/{path} b/{path}\n");

    match (old.mode, new.mode) {
        (None, Some(new_mode)) => {
            header.push_str(&format!("new file mode {new_mode:o}\n"));
            if content_changed {
                header.push_str(&format!("index 0000000..{new_hash}\n"));
            }
        }
        (Some(old_mode), None) => {
            header.push_str(&format!("deleted file mode {old_mode:o}\n"));
            if content_changed {
                header.push_str(&format!("index {old_hash}..0000000\n"));
            }
        }
        (Some(old_mode), Some(new_mode)) => {
            if old_mode != new_mode {
                header.push_str(&format!("old mode {old_mode:o}\n"));
                header.push_str(&format!("new mode {new_mode:o}\n"));
            }
            if content_changed {
                if old_mode == new_mode {
                    header.push_str(&format!("index {old_hash}..{new_hash} {new_mode:o}\n"));
                } else {
                    header.push_str(&format!("index {old_hash}..{new_hash}\n"));
                }
            }
        }
        (None, None) => {
            return Err(AppError::system(
                "git diff change has neither an old nor a new side",
            ));
        }
    }

    if include_file_markers {
        let old_path = if has_old {
            format!("a/{path}")
        } else {
            "/dev/null".to_string()
        };
        let new_path = if has_new {
            format!("b/{path}")
        } else {
            "/dev/null".to_string()
        };
        header.push_str(&format!("--- {old_path}\n+++ {new_path}\n"));
    }
    Ok(header)
}

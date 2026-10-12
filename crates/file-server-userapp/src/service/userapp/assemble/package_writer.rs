//! 所有整体包条目共用一次登记；仅重名时读取已写入的内容，保留首次条目。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use file_server::error::{AppError, AppResult};

type ZipWriterFile = ::zip::ZipWriter<std::fs::File>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Symlink,
}

struct PackedEntry {
    kind: EntryKind,
    origin: String,
}

enum PackageOutput {
    Writing(Box<ZipWriterFile>),
    Readable(::zip::ZipArchive<std::fs::File>),
}

pub(super) struct PackageWriter {
    output: Option<PackageOutput>,
    entries: HashMap<String, PackedEntry>,
}

impl PackageWriter {
    pub(super) fn new(file: std::fs::File) -> Self {
        Self {
            output: Some(PackageOutput::Writing(Box::new(ZipWriterFile::new(file)))),
            entries: HashMap::new(),
        }
    }

    pub(super) fn copy_artifact_entry<R: Read>(
        &mut self,
        mut entry: ::zip::read::ZipFile<'_, R>,
        name: &str,
        origin: String,
    ) -> AppResult<()> {
        if self.is_duplicate(name, EntryKind::File, &mut entry, &origin)? {
            return Ok(());
        }
        self.writer_mut()?
            .raw_copy_file_rename(entry, name)
            .map_err(|e| AppError::file(format!("raw_copy {name} from {origin}: {e}")))?;
        self.record(name, EntryKind::File, origin);
        Ok(())
    }

    pub(super) fn add_symlink(
        &mut self,
        name: &str,
        target: &str,
        origin: String,
    ) -> AppResult<()> {
        if self.is_duplicate(name, EntryKind::Symlink, &mut Cursor::new(target), &origin)? {
            return Ok(());
        }
        self.writer_mut()?
            .add_symlink(name, target, super::deflate_opts())
            .map_err(|e| AppError::file(format!("copy artifact symbolic link {name}: {e}")))?;
        self.record(name, EntryKind::Symlink, origin);
        Ok(())
    }

    pub(super) fn add_file(&mut self, name: &str, path: &Path) -> AppResult<()> {
        let origin = path.display().to_string();
        let mut file = std::fs::File::open(path)
            .map_err(|e| AppError::file(format!("open package source {origin}: {e}")))?;
        self.write_entry(name, &mut file, origin)
    }

    pub(super) fn add_bytes(&mut self, name: &str, bytes: &[u8], origin: &str) -> AppResult<()> {
        self.write_entry(name, &mut Cursor::new(bytes), origin.to_owned())
    }

    pub(super) fn finish(mut self) -> AppResult<()> {
        if let PackageOutput::Writing(writer) = self.take_output()? {
            (*writer)
                .finish()
                .map_err(|e| AppError::file(format!("zip finish: {e}")))?;
        }
        // Readable 已经由 finish_into_readable 完成中央目录；最后几项去重不需重写。
        Ok(())
    }

    fn write_entry(
        &mut self,
        name: &str,
        contents: &mut impl Read,
        origin: String,
    ) -> AppResult<()> {
        if self.is_duplicate(name, EntryKind::File, contents, &origin)? {
            return Ok(());
        }
        let writer = self.writer_mut()?;
        writer
            .start_file(name, super::deflate_opts())
            .map_err(|e| AppError::file(format!("start_file {name} from {origin}: {e}")))?;
        std::io::copy(contents, writer).map_err(|e| {
            AppError::file(format!("write package entry {name} from {origin}: {e}"))
        })?;
        self.record(name, EntryKind::File, origin);
        Ok(())
    }

    fn is_duplicate(
        &mut self,
        name: &str,
        kind: EntryKind,
        contents: &mut impl Read,
        origin: &str,
    ) -> AppResult<bool> {
        let Some(previous) = self.entries.get(name) else {
            return Ok(false);
        };
        let previous_origin = previous.origin.clone();
        if previous.kind != kind {
            return Err(entry_conflict(
                name,
                &previous_origin,
                origin,
                "entry types",
            ));
        }

        // 比较包内已写入的字节，不能重新读取旧源文件：源文件可能已被修改。
        // 未重名条目不解压、不哈希，继续 raw-copy；只有重名才检查内容。
        // 连续重复条目共用同一读取阶段，不为每项重复刷新整个中央目录。
        let archive = self.readable_mut()?;
        let equal = {
            let mut previous = archive
                .by_name(name)
                .map_err(|e| AppError::file(format!("read packed entry {name}: {e}")))?;
            streams_equal(&mut previous, contents)
                .map_err(|e| AppError::file(format!("compare package entry {name}: {e}")))?
        };
        if !equal {
            return Err(entry_conflict(name, &previous_origin, origin, "contents"));
        }
        Ok(true)
    }

    fn record(&mut self, name: &str, kind: EntryKind, origin: String) {
        self.entries
            .insert(name.to_owned(), PackedEntry { kind, origin });
    }

    fn writer_mut(&mut self) -> AppResult<&mut ZipWriterFile> {
        let writer = match self.take_output()? {
            PackageOutput::Writing(writer) => writer,
            PackageOutput::Readable(archive) => Box::new(resume_writer(archive)?),
        };
        self.output = Some(PackageOutput::Writing(writer));
        match self.output.as_mut() {
            Some(PackageOutput::Writing(writer)) => Ok(writer.as_mut()),
            _ => Err(writer_unavailable()),
        }
    }

    fn readable_mut(&mut self) -> AppResult<&mut ::zip::ZipArchive<std::fs::File>> {
        let archive = match self.take_output()? {
            PackageOutput::Readable(archive) => archive,
            PackageOutput::Writing(writer) => (*writer)
                .finish_into_readable()
                .map_err(|e| AppError::file(format!("open partial package for comparison: {e}")))?,
        };
        self.output = Some(PackageOutput::Readable(archive));
        match self.output.as_mut() {
            Some(PackageOutput::Readable(archive)) => Ok(archive),
            _ => Err(writer_unavailable()),
        }
    }

    fn take_output(&mut self) -> AppResult<PackageOutput> {
        self.output.take().ok_or_else(writer_unavailable)
    }
}

fn writer_unavailable() -> AppError {
    AppError::system("package writer is unavailable after a failed write")
}

fn resume_writer(archive: ::zip::ZipArchive<std::fs::File>) -> AppResult<ZipWriterFile> {
    let central_start = archive.central_directory_start();
    let file = archive.into_inner();
    // File::try_clone 共用文件游标。仅在此单线程、writer尚未开始新条目时恢复游标。
    let mut cursor = file
        .try_clone()
        .map_err(|e| AppError::file(format!("clone partial package handle: {e}")))?;
    let writer = ZipWriterFile::new_append(file)
        .map_err(|e| AppError::file(format!("resume partial package: {e}")))?;
    // new_append 读完旧中央目录后不会退回其起点；必须覆盖它，避免包体不断增长。
    // 旧条目元数据已在writer内；只截掉临时包的目录/尾部，保留全部payload字节。
    cursor
        .set_len(central_start)
        .map_err(|e| AppError::file(format!("truncate temporary package directory: {e}")))?;
    cursor
        .seek(SeekFrom::Start(central_start))
        .map_err(|e| AppError::file(format!("restore temporary package append position: {e}")))?;
    Ok(writer)
}

fn entry_conflict(name: &str, previous: &str, incoming: &str, reason: &str) -> AppError {
    AppError::file(format!(
        "workspace package entry conflict at {name}: different {reason}; first source: {previous}; incoming source: {incoming}"
    ))
}

fn streams_equal(first: &mut impl Read, second: &mut impl Read) -> std::io::Result<bool> {
    let mut first = BufReader::new(first);
    let mut second = BufReader::new(second);
    loop {
        let first_bytes = first.fill_buf()?;
        let second_bytes = second.fill_buf()?;
        if first_bytes.is_empty() || second_bytes.is_empty() {
            return Ok(first_bytes.is_empty() && second_bytes.is_empty());
        }
        let count = first_bytes.len().min(second_bytes.len());
        if first_bytes[..count] != second_bytes[..count] {
            return Ok(false);
        }
        first.consume(count);
        second.consume(count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ShortRead<'a> {
        remaining: &'a [u8],
        limit: usize,
    }

    impl Read for ShortRead<'_> {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            let count = output.len().min(self.limit).min(self.remaining.len());
            output[..count].copy_from_slice(&self.remaining[..count]);
            self.remaining = &self.remaining[count..];
            Ok(count)
        }
    }

    #[test]
    fn comparison_handles_empty_content_and_asymmetric_short_reads() {
        for size in [0, 1, 8191, 8192, 8193, 20000] {
            let bytes = vec![b'x'; size];
            let mut first = ShortRead {
                remaining: &bytes,
                limit: 3,
            };
            let mut second = ShortRead {
                remaining: &bytes,
                limit: 11,
            };
            assert!(streams_equal(&mut first, &mut second).unwrap());
            let mut longer = bytes.clone();
            longer.push(b'x');
            assert!(!streams_equal(&mut Cursor::new(&bytes), &mut Cursor::new(longer)).unwrap());
        }
    }

    #[test]
    fn comparison_reads_packed_bytes_after_original_source_changes() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("original.sql");
        let equal_source = root.path().join("equal.sql");
        std::fs::write(&source, b"SELECT 'original';\n").unwrap();
        std::fs::write(&equal_source, b"SELECT 'original';\n").unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(root.path().join("package.zip"))
            .unwrap();
        let mut package = PackageWriter::new(file);
        package.add_file("database/001.sql", &source).unwrap();
        std::fs::write(&source, b"SELECT 'modified';\n").unwrap();
        package.add_file("database/001.sql", &equal_source).unwrap();
        let error = package
            .add_file("database/001.sql", &source)
            .expect_err("later mutations must not redefine bytes already in the package");
        assert!(error.to_string().contains("different contents"));
    }
}

//! Atomic receipt replacement, including bounded Windows sharing contention.
//!
//! Never remove the destination before replacement: readers must see either
//! complete record, and a failed write must preserve the last durable evidence.
use std::{fs::File, io, path::Path};

pub fn persist(file: tempfile::NamedTempFile, path: &Path) -> io::Result<File> {
    #[cfg(not(windows))]
    {
        file.persist(path).map_err(|error| error.error)
    }
    #[cfg(windows)]
    {
        let mut file = file;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            match file.persist(path) {
                Ok(file) => return Ok(file),
                Err(error) => {
                    // ERROR_ACCESS_DENIED / SHARING_VIOLATION / LOCK_VIOLATION.
                    // Windows may report ACCESS_DENIED for a replacing rename
                    // while another handle is still closing. A permanent ACL
                    // failure remains an error after this small fixed budget.
                    if matches!(error.error.raw_os_error(), Some(5 | 32 | 33))
                        && std::time::Instant::now() < deadline
                    {
                        file = error.file;
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    } else {
                        return Err(error.error);
                    }
                }
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::{io::Write, os::windows::fs::OpenOptionsExt, time::Duration};

    #[test]
    fn sharing_contention_retries_but_keeps_old_record_on_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipt.json");
        std::fs::write(&path, b"old").unwrap();
        let reader = File::options()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            drop(reader);
        });
        let mut new = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        new.write_all(b"new").unwrap();
        new.as_file().sync_all().unwrap();
        persist(new, &path).unwrap();
        release.join().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");

        let _reader = File::options()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        let mut rejected = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        rejected.write_all(b"must not replace").unwrap();
        let started = std::time::Instant::now();
        assert!(persist(rejected, &path).is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }
}

//! 整体包真实组装反例：子 ZIP 已含 database/ 时，不得重复写同名 SQL。

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use shared_types::{LockedPingap, PingapMode};

use super::{BuiltProject, ReleaseLock, assemble_workspace_package};

const PROJECT: &str = "lanproxy-service";
const SQL_REL: &str = "database/001_lanproxy_schema.sql";
const SQL_PACKAGE_PATH: &str = "lanproxy-service/database/001_lanproxy_schema.sql";
const PACKAGE_REL: &str = "builds/workspace-package-overlap-release.zip";

fn release_lock() -> ReleaseLock {
    ReleaseLock {
        schema_version: 1,
        release_id: "overlap-release".into(),
        workspace_name: "overlap-regression".into(),
        pingap: LockedPingap {
            mode: PingapMode::Managed,
            config: None,
            version: "test".into(),
            commit: "test".into(),
        },
        minimum_app_cli_version: "0.1.3".into(),
        runtime_image_digest: "sha256:test".into(),
        services: Vec::new(),
        bridge_service: None,
    }
}

fn write_source(ws: &Path, rel: &str, bytes: &[u8]) {
    let path = ws.join(PROJECT).join(rel);
    std::fs::create_dir_all(path.parent().expect("source parent")).expect("source directory");
    std::fs::write(path, bytes).expect("source file");
}

fn artifact(ws: &Path, entries: &[(&str, &[u8])]) -> BuiltProject {
    let path = ws.join(PROJECT).join("artifact.zip");
    std::fs::create_dir_all(path.parent().expect("artifact parent")).expect("project directory");
    write_zip(&path, entries);
    BuiltProject {
        path: PROJECT.into(),
        artifact: path,
        artifact_rel: "artifact.zip".into(),
    }
}

fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("ZIP parent");
    }
    let mut writer = ::zip::ZipWriter::new(std::fs::File::create(path).expect("ZIP output"));
    // 用非默认压缩级别和可执行权限，组包必须保留原 raw 数据及元信息。
    let options = ::zip::write::SimpleFileOptions::default()
        .compression_method(::zip::CompressionMethod::Deflated)
        .compression_level(Some(1))
        .unix_permissions(0o751);
    for (name, bytes) in entries {
        writer.start_file(*name, options).expect("ZIP entry");
        writer.write_all(bytes).expect("ZIP bytes");
    }
    writer.finish().expect("finish ZIP");
}

#[derive(Debug, PartialEq)]
struct EntrySnapshot {
    bytes: Vec<u8>,
    raw_compressed_bytes: Vec<u8>,
    compression: ::zip::CompressionMethod,
    unix_mode: Option<u32>,
    crc32: u32,
    is_symlink: bool,
}

fn snapshot(path: &Path, entry_name: &str) -> EntrySnapshot {
    let mut archive =
        ::zip::ZipArchive::new(std::fs::File::open(path).expect("open ZIP")).expect("read ZIP");
    let mut entry = archive.by_name(entry_name).expect("named ZIP entry");
    let compression = entry.compression();
    let unix_mode = entry.unix_mode();
    let crc32 = entry.crc32();
    let is_symlink = entry.is_symlink();
    let data_start = entry.data_start().expect("entry data offset");
    let compressed_size = entry.compressed_size();
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes).expect("uncompressed entry");

    let mut raw_file = std::fs::File::open(path).expect("raw ZIP");
    raw_file
        .seek(SeekFrom::Start(data_start))
        .expect("raw data seek");
    let mut raw_compressed_bytes = Vec::new();
    raw_file
        .take(compressed_size)
        .read_to_end(&mut raw_compressed_bytes)
        .expect("raw compressed data");
    assert_eq!(raw_compressed_bytes.len() as u64, compressed_size);
    EntrySnapshot {
        bytes,
        raw_compressed_bytes,
        compression,
        unix_mode,
        crc32,
        is_symlink,
    }
}

fn assert_unique_entries(path: &Path, expected_path: &str) {
    let mut archive =
        ::zip::ZipArchive::new(std::fs::File::open(path).expect("open ZIP")).expect("read ZIP");
    let mut names = HashSet::new();
    let mut expected_count = 0;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).expect("ZIP entry");
        if entry.name().unwrap() == expected_path {
            expected_count += 1;
        }
        assert!(
            names.insert(entry.name().unwrap().into_owned()),
            "duplicate package entry: {}",
            entry.name().unwrap()
        );
    }
    assert_eq!(
        expected_count, 1,
        "the expected file must occur exactly once"
    );
}

fn seed_published_package(ws: &Path) -> Vec<u8> {
    let out = ws.join(PACKAGE_REL);
    write_zip(&out, &[("old-release.txt", b"previous complete release")]);
    std::fs::read(out).expect("old published ZIP bytes")
}

fn assert_old_package_and_no_part(ws: &Path, old_bytes: &[u8]) {
    let out = ws.join(PACKAGE_REL);
    assert_eq!(std::fs::read(&out).expect("preserved package"), old_bytes);
    assert_eq!(
        snapshot(&out, "old-release.txt").bytes,
        b"previous complete release"
    );
    assert!(
        !out.with_extension("zip.part").exists(),
        "failed .part must be removed"
    );
}

#[tokio::test]
async fn assemble_overlap_identical_child_zip_and_source_sql_is_written_once() {
    let ws = tempfile::tempdir().expect("workspace");
    let sql = b"CREATE TABLE IF NOT EXISTS lanproxy (id BIGINT PRIMARY KEY);\n".repeat(200);
    write_source(ws.path(), SQL_REL, &sql);
    let built = artifact(ws.path(), &[(SQL_REL, &sql), ("start.sh", b"#!/bin/sh\n")]);
    let source_entry = snapshot(&built.artifact, SQL_REL);
    let source_zip_before = std::fs::read(&built.artifact).expect("artifact before");
    let out = assemble_workspace_package(
        ws.path(),
        std::slice::from_ref(&built),
        &release_lock(),
        PACKAGE_REL,
    )
    .await
    .expect("identical SQL overlap must assemble");

    assert_unique_entries(&out, SQL_PACKAGE_PATH);
    assert_eq!(snapshot(&out, SQL_PACKAGE_PATH), source_entry);
    assert_eq!(snapshot(&out, SQL_PACKAGE_PATH).bytes, sql);
    assert_eq!(
        std::fs::read(&built.artifact).expect("artifact after"),
        source_zip_before,
        "assembling must not modify the child artifact"
    );
    assert_eq!(
        std::fs::read(ws.path().join(PROJECT).join(SQL_REL)).unwrap(),
        sql
    );
    assert!(!out.with_extension("zip.part").exists());
}

#[tokio::test]
async fn assemble_overlap_different_sql_reports_both_origins_and_preserves_old_package() {
    let ws = tempfile::tempdir().expect("workspace");
    let old_bytes = seed_published_package(ws.path());
    write_source(ws.path(), SQL_REL, b"SELECT 'source';\n");
    let built = artifact(ws.path(), &[(SQL_REL, b"SELECT 'artifact';\n")]);
    let artifact_path = built.artifact.to_string_lossy().into_owned();
    let source_path = ws
        .path()
        .join(PROJECT)
        .join(SQL_REL)
        .to_string_lossy()
        .into_owned();
    let error = assemble_workspace_package(ws.path(), &[built], &release_lock(), PACKAGE_REL)
        .await
        .expect_err("different SQL content at the same package path must conflict");
    let message = error.to_string();
    assert!(
        message.contains(SQL_PACKAGE_PATH),
        "conflicting entry: {message}"
    );
    assert!(
        message.contains(&artifact_path),
        "artifact origin: {message}"
    );
    assert!(message.contains(&source_path), "source origin: {message}");
    assert_old_package_and_no_part(ws.path(), &old_bytes);
}

#[tokio::test]
async fn assemble_overlap_keeps_sql_only_present_in_source() {
    let ws = tempfile::tempdir().expect("workspace");
    write_source(ws.path(), SQL_REL, b"SELECT 'source only';\n");
    let built = artifact(ws.path(), &[("server.jar", b"application artifact")]);
    let out = assemble_workspace_package(ws.path(), &[built], &release_lock(), PACKAGE_REL)
        .await
        .expect("source SQL supplement");
    assert_unique_entries(&out, SQL_PACKAGE_PATH);
    assert_eq!(
        snapshot(&out, SQL_PACKAGE_PATH).bytes,
        b"SELECT 'source only';\n"
    );
    assert_eq!(
        snapshot(&out, "lanproxy-service/server.jar").bytes,
        b"application artifact"
    );
}

#[tokio::test]
async fn assemble_overlap_symlink_and_regular_file_with_same_bytes_conflict() {
    let ws = tempfile::tempdir().expect("workspace");
    let old_bytes = seed_published_package(ws.path());
    let target = "001_schema_real.sql";
    write_source(ws.path(), SQL_REL, target.as_bytes());
    let built = artifact(
        ws.path(),
        &[("database/001_schema_real.sql", b"SELECT 1;\n")],
    );
    let mut zip = ::zip::ZipWriter::new(std::fs::File::create(&built.artifact).expect("ZIP"));
    zip.add_symlink(SQL_REL, target, ::zip::write::SimpleFileOptions::default())
        .expect("safe symlink entry");
    zip.finish().expect("finish symlink ZIP");
    assert!(snapshot(&built.artifact, SQL_REL).is_symlink);
    assert_eq!(snapshot(&built.artifact, SQL_REL).bytes, target.as_bytes());
    let artifact_path = built.artifact.to_string_lossy().into_owned();
    let source_path = ws
        .path()
        .join(PROJECT)
        .join(SQL_REL)
        .to_string_lossy()
        .into_owned();

    let error = assemble_workspace_package(ws.path(), &[built], &release_lock(), PACKAGE_REL)
        .await
        .expect_err("equal bytes do not make different entry types equivalent");
    let message = error.to_string();
    assert!(message.contains(SQL_PACKAGE_PATH));
    assert!(
        message.contains(&artifact_path),
        "symlink artifact origin: {message}"
    );
    assert!(
        message.contains(&source_path),
        "regular file origin: {message}"
    );
    assert_old_package_and_no_part(ws.path(), &old_bytes);
}

#[tokio::test]
async fn assemble_overlap_raw_copy_keeps_unrelated_compressed_bytes_and_permissions() {
    let ws = tempfile::tempdir().expect("workspace");
    let contents = b"console.log('raw artifact stays compressed');\n".repeat(300);
    let built = artifact(ws.path(), &[("nested/server.js", &contents)]);
    let source = snapshot(&built.artifact, "nested/server.js");
    assert_eq!(source.unix_mode.map(|mode| mode & 0o777), Some(0o751));
    assert_eq!(source.compression, ::zip::CompressionMethod::Deflated);
    let out = assemble_workspace_package(ws.path(), &[built], &release_lock(), PACKAGE_REL)
        .await
        .expect("unrelated raw artifact assembly");
    assert_unique_entries(&out, "lanproxy-service/nested/server.js");
    assert_eq!(snapshot(&out, "lanproxy-service/nested/server.js"), source);
}

#[tokio::test]
async fn assemble_overlap_repeated_entries_then_new_files_keep_archive_size_bounded() {
    let ws = tempfile::tempdir().expect("workspace");
    let no_overlap = tempfile::tempdir().expect("equivalent workspace without source duplicates");
    let names: Vec<String> = (0..20)
        .map(|i| format!("database/{i:03}_schema.sql"))
        .collect();
    let contents = b"CREATE TABLE IF NOT EXISTS t (id BIGINT);\n";
    for name in &names {
        write_source(ws.path(), name, contents);
    }
    let entries: Vec<(&str, &[u8])> = names
        .iter()
        .map(|name| (name.as_str(), contents.as_slice()))
        .collect();
    let first = artifact(ws.path(), &entries);
    let first_without_overlap = artifact(no_overlap.path(), &entries);

    // BuiltProject 顺序保证 duplicate 收集结束后还有一个新文件需要写入。
    let make_later = |root: &Path| {
        let zip = root.join("later/artifact.zip");
        write_zip(&zip, &[("server.jar", b"later service")]);
        let sql = root.join("later/database/999_later.sql");
        std::fs::create_dir_all(sql.parent().unwrap()).unwrap();
        std::fs::write(&sql, b"SELECT 'later';\n").unwrap();
        BuiltProject {
            path: "later".into(),
            artifact: zip,
            artifact_rel: "artifact.zip".into(),
        }
    };
    let later = make_later(ws.path());
    let later_without_overlap = make_later(no_overlap.path());
    let out = assemble_workspace_package(ws.path(), &[first, later], &release_lock(), PACKAGE_REL)
        .await
        .expect("multiple identical overlaps followed by a new file");
    let expected = assemble_workspace_package(
        no_overlap.path(),
        &[first_without_overlap, later_without_overlap],
        &release_lock(),
        PACKAGE_REL,
    )
    .await
    .expect("equivalent output with no duplicate sources");

    for name in &names {
        let packed_name = format!("{PROJECT}/{name}");
        assert_unique_entries(&out, &packed_name);
        assert_eq!(snapshot(&out, &packed_name).bytes, contents);
    }
    assert_unique_entries(&out, "later/database/999_later.sql");
    assert_eq!(
        snapshot(&out, "later/database/999_later.sql").bytes,
        b"SELECT 'later';\n"
    );
    assert_eq!(
        std::fs::metadata(&out).unwrap().len(),
        std::fs::metadata(&expected).unwrap().len(),
        "duplicate comparisons must not leave obsolete central directories in the package"
    );
}

#[tokio::test]
async fn assemble_overlap_same_length_different_sql_is_not_overwritten() {
    let ws = tempfile::tempdir().expect("workspace");
    let old_bytes = seed_published_package(ws.path());
    let source = b"SELECT 'aaaa';\n";
    let packed = b"SELECT 'bbbb';\n";
    assert_eq!(source.len(), packed.len());
    write_source(ws.path(), SQL_REL, source);
    let built = artifact(ws.path(), &[(SQL_REL, packed)]);
    let error = assemble_workspace_package(ws.path(), &[built], &release_lock(), PACKAGE_REL)
        .await
        .expect_err("size alone cannot prove identical SQL contents");
    assert!(error.to_string().contains(SQL_PACKAGE_PATH));
    assert!(error.to_string().contains("different contents"));
    assert_old_package_and_no_part(ws.path(), &old_bytes);
}

#[tokio::test]
async fn assemble_overlap_same_basename_in_different_projects_keeps_both() {
    let ws = tempfile::tempdir().expect("workspace");
    write_source(ws.path(), SQL_REL, b"SELECT 'first';\n");
    let first = artifact(ws.path(), &[(SQL_REL, b"SELECT 'first';\n")]);
    let second_zip = ws.path().join("second/artifact.zip");
    write_zip(&second_zip, &[(SQL_REL, b"SELECT 'second';\n")]);
    let second_source = ws.path().join("second").join(SQL_REL);
    std::fs::create_dir_all(second_source.parent().unwrap()).unwrap();
    std::fs::write(second_source, b"SELECT 'second';\n").unwrap();
    let second = BuiltProject {
        path: "second".into(),
        artifact: second_zip,
        artifact_rel: "artifact.zip".into(),
    };
    let out = assemble_workspace_package(ws.path(), &[first, second], &release_lock(), PACKAGE_REL)
        .await
        .expect("deduplication is scoped to the full destination path");
    assert_unique_entries(&out, SQL_PACKAGE_PATH);
    assert_unique_entries(&out, "second/database/001_lanproxy_schema.sql");
    assert_eq!(snapshot(&out, SQL_PACKAGE_PATH).bytes, b"SELECT 'first';\n");
    assert_eq!(
        snapshot(&out, "second/database/001_lanproxy_schema.sql").bytes,
        b"SELECT 'second';\n"
    );
}

#[test]
fn assemble_overlap_alternating_raw_copy_and_duplicates_preserve_payloads() {
    use super::package_writer::PackageWriter;

    let ws = tempfile::tempdir().unwrap();
    let entries = [
        ("one.sql", b"SELECT 'one';\n".as_slice()),
        ("two.sql", b"SELECT 'two';\n".as_slice()),
        ("three.sql", b"SELECT 'three';\n".as_slice()),
    ];
    let source = ws.path().join("source.zip");
    write_zip(&source, &entries);
    let make_package = |duplicates: bool, output: &Path| {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(output)
            .unwrap();
        let mut writer = PackageWriter::new(file);
        let mut artifact = ::zip::ZipArchive::new(std::fs::File::open(&source).unwrap()).unwrap();
        for (name, bytes) in entries {
            writer
                .copy_artifact_entry(artifact.by_name(name).unwrap(), name, "source.zip".into())
                .unwrap();
            if duplicates {
                writer.add_bytes(name, bytes, "duplicate source").unwrap();
            }
        }
        writer
            .add_bytes("trailing.txt", b"last file", "generated")
            .unwrap();
        writer.finish().unwrap();
    };
    let out = ws.path().join("overlap.zip");
    let expected = ws.path().join("without-overlap.zip");
    make_package(true, &out);
    make_package(false, &expected);
    for (name, _) in entries {
        assert_unique_entries(&out, name);
        assert_eq!(snapshot(&out, name), snapshot(&source, name));
    }
    assert_eq!(snapshot(&out, "trailing.txt").bytes, b"last file");
    assert_eq!(
        std::fs::metadata(out).unwrap().len(),
        std::fs::metadata(expected).unwrap().len()
    );
}

//! 版本管理: 备份/恢复/版本 zip 路径 (对齐 nuwax `backupUtils`)。
//!
//! 版本 zip 路径: `UPLOAD_PROJECT_DIR/{projectId}/{projectId}-v{N}.zip`

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::workspace::{ProjectContext, WorkspaceResolver};

/// codeVersion 字符串 → u64 (对齐 nuwax: 须为有限正数)。
pub fn parse_version(code_version: &str) -> AppResult<u64> {
    code_version
        .trim()
        .parse::<u64>()
        .map_err(|_| AppError::validation("Code version must be a number"))
}

/// 版本 zip 路径: `UPLOAD_PROJECT_DIR/{projectId}/{projectId}-v{N}.zip`
pub fn version_zip_path(config: &Config, project_id: &str, version: u64) -> PathBuf {
    config
        .upload_project_dir
        .join(project_id)
        .join(format!("{project_id}-v{version}.zip"))
}

/// 恢复来源核验结论 (P1-3/FS-04): 区分缺失/确切损坏/观察失败——权限或读取
/// 错误保留具体原因, 不静默折叠成"不存在"。
#[derive(Debug)]
enum RestoreSourceVerdict {
    /// 全部 entry 读取到 EOF 且 CRC 通过（local header 与压缩方法由读取覆盖）。
    Verified,
    /// 文件不存在。
    Missing,
    /// 中央目录可读但 payload/CRC/条目损坏——不可作为恢复来源。
    Corrupt(String),
    /// 打开/读取观察失败（权限、I/O）——原因如文。
    InspectFailed(String),
}

/// 完整核验恢复来源: 逐 entry `read_to_end`（zip crate 在实际读取到 EOF 时校验
/// CRC; `ZipArchive::new` 只信中央目录, 不能证明可恢复）。同步阻塞 IO, 由
/// 调用方放入 blocking worker。
fn verify_restore_source_blocking(zip_path: &Path) -> RestoreSourceVerdict {
    let file = match std::fs::File::open(zip_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return RestoreSourceVerdict::Missing;
        }
        Err(error) => {
            return RestoreSourceVerdict::InspectFailed(format!(
                "open {}: {error}",
                zip_path.display()
            ));
        }
    };
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(archive) => archive,
        Err(error) => {
            return RestoreSourceVerdict::Corrupt(format!(
                "parse central directory of {}: {error}",
                zip_path.display()
            ));
        }
    };
    for index in 0..archive.len() {
        let mut entry = match archive.by_index(index) {
            Ok(entry) => entry,
            Err(error) => {
                return RestoreSourceVerdict::Corrupt(format!(
                    "read entry {index} of {}: {error}",
                    zip_path.display()
                ));
            }
        };
        let mut sink = std::io::sink();
        if let Err(error) = std::io::copy(&mut entry, &mut sink) {
            return RestoreSourceVerdict::Corrupt(format!(
                "verify payload of entry '{}' in {}: {error}",
                entry.name(),
                zip_path.display()
            ));
        }
    }
    RestoreSourceVerdict::Verified
}

async fn verify_restore_source(zip_path: &Path) -> RestoreSourceVerdict {
    let zip_path = zip_path.to_path_buf();
    match tokio::task::spawn_blocking(move || verify_restore_source_blocking(&zip_path)).await {
        Ok(verdict) => verdict,
        Err(error) => {
            RestoreSourceVerdict::InspectFailed(format!("verification worker interrupted: {error}"))
        }
    }
}

/// 备份项目到版本 zip (非 GIT 模式); `GIT_ENABLED` 时跳过返回 `None`。
pub async fn backup_project(
    config: &Config,
    project_id: &str,
    project_path: &Path,
    code_version: &str,
) -> AppResult<Option<PathBuf>> {
    if config.git_enabled {
        return Ok(None);
    }
    let version = parse_version(code_version)?;
    let zip_path = version_zip_path(config, project_id, version);
    crate::service::zip::pack_dir(
        project_path.to_path_buf(),
        zip_path.clone(),
        config.traverse_exclude_dirs.clone(),
        config.backup_traverse_exclude_files.clone(),
    )
    .await?;
    Ok(Some(zip_path))
}

/// 从版本 zip 恢复 (P1-3/FS-04 加固):
/// 1. 与原子发布共用同一 pack 锁——核验到提取期间该路径不会被并发发布替换
///    （受保护快照等价）;
/// 2. 锁内完整核验（逐 entry 读到 EOF + CRC）——中央目录可读不等于可恢复;
/// 3. 先解压到项目同卷 staging, 全部成功后才清空项目并落位——核验/提取任何
///    失败发生在破坏性清理**之前**, 项目原样保留;
/// 4. 清空时保留 excluded 目录 (TRAVERSE_EXCLUDE_DIRS, 如 node_modules/.git) 与
///    excluded 文件 (BACKUP_TRAVERSE_EXCLUDE_FILES, 如 lock 文件)——版本 zip 不含
///    这些条目, 保留可避免 rollback 后重装依赖。
pub async fn restore_from_zip(
    project_path: &Path,
    zip_path: &Path,
    exclude_dirs: &[String],
    exclude_files: &[String],
) -> AppResult<()> {
    let _pack_guard = crate::service::zip::acquire_pack_lock(zip_path).map_err(|error| {
        AppError::system(format!(
            "lock restore source {}: {error}",
            zip_path.display()
        ))
    })?;
    match verify_restore_source(zip_path).await {
        RestoreSourceVerdict::Verified => {}
        RestoreSourceVerdict::Missing => {
            return Err(AppError::resource(format!(
                "restore source {} not found",
                zip_path.display()
            )));
        }
        RestoreSourceVerdict::Corrupt(reason) => {
            return Err(AppError::resource(format!(
                "restore source {} is corrupt ({reason}); project left untouched",
                zip_path.display()
            )));
        }
        RestoreSourceVerdict::InspectFailed(reason) => {
            return Err(AppError::system(format!(
                "inspect restore source {}: {reason}",
                zip_path.display()
            )));
        }
    }
    // 同卷 staging: 提取全部成功之前不动项目目录。
    let staging_parent = project_path.parent().unwrap_or(project_path).to_path_buf();
    let staging = tempfile::Builder::new()
        .prefix(".restore-staging-")
        .tempdir_in(&staging_parent)
        .map_err(|error| {
            AppError::system(format!(
                "create staging near {}: {error}",
                project_path.display()
            ))
        })?;
    crate::service::zip::extract_to(zip_path.to_path_buf(), staging.path().to_path_buf()).await?;
    // staging 全量成功 → 清空（保留 excluded）→ 落位。
    clear_dir_keep_excluded(project_path, exclude_dirs, exclude_files).await?;
    let mut rd = fs::read_dir(staging.path()).await?;
    while let Some(entry) = rd.next_entry().await? {
        let source = entry.path();
        let target = project_path.join(entry.file_name());
        if let Err(error) = fs::rename(&source, &target).await {
            #[cfg(unix)]
            let cross_device = error.raw_os_error() == Some(nix::libc::EXDEV);
            #[cfg(not(unix))]
            let cross_device = error.raw_os_error() == Some(17);
            if cross_device {
                // 同卷 staging 下不应发生; 保守退化为 copy。
                copy_tree(&source, &target).await?;
                drop(fs::remove_dir_all(&source).await);
            } else {
                return Err(AppError::system(format!(
                    "move staged entry {} into project: {error}",
                    source.display()
                )));
            }
        }
    }
    Ok(())
}

/// 跨设备退化用整树复制（staging 同卷下理论不可达, 保守兜底）;
/// 已有 `fs_util::copy_dir_filtered` 提供同步整树复制, 直接复用。
async fn copy_tree(source: &Path, target: &Path) -> AppResult<()> {
    if source.is_dir() {
        crate::service::fs_util::copy_dir_filtered(source, target, &[], &[]).await?;
    } else {
        fs::copy(source, target)
            .await
            .map_err(|error| AppError::system(format!("copy {source:?}: {error}")))?;
    }
    Ok(())
}

/// 清空目录内容, 但保留名字命中 exclude_dirs (目录) / exclude_files (文件) 的顶层条目
/// (对齐 nuwax restoreProjectFromZip 的 "keep excluded" 清理)。
/// 删除失败必须中止恢复，避免旧文件残留后继续覆盖成混合版本。
async fn clear_dir_keep_excluded(
    dir: &Path,
    exclude_dirs: &[String],
    exclude_files: &[String],
) -> AppResult<()> {
    let mut rd = fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        let ft = entry.file_type().await?;
        if ft.is_dir() && exclude_dirs.iter().any(|d| d == &name) {
            continue;
        }
        if ft.is_file() && exclude_files.iter().any(|f| f == &name) {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            fs::remove_dir_all(&path).await
        } else {
            fs::remove_file(&path).await
        }
        .map_err(|error| {
            AppError::system(format!(
                "clear project entry before restore {}: {error}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

// ── backup-current-version ──────────────────────────────────────────────────────

pub struct BackupVersionResult {
    pub project_id: String,
    pub zip_path: String,
}

/// 备份当前版本到 `{projectId}-v{N}.zip` (GIT_ENABLED 由 handler 拦截为 deprecated)。
pub async fn backup_current_version(
    resolver: &dyn WorkspaceResolver,
    config: &Config,
    ctx: &ProjectContext,
    code_version: &str,
) -> AppResult<BackupVersionResult> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    let project_path = resolver.resolve_project(ctx).await?;
    if !crate::service::fs_util::path_exists(&project_path).await? {
        return Err(AppError::resource("Project does not exist"));
    }
    let zip = backup_project(config, project_id, &project_path, code_version)
        .await?
        .ok_or_else(|| AppError::business("backup disabled in GIT mode"))?;
    Ok(BackupVersionResult {
        project_id: project_id.to_string(),
        zip_path: zip.to_string_lossy().to_string(),
    })
}

// ── rollback-version ────────────────────────────────────────────────────────────

pub struct RollbackResult {
    pub new_version: u64,
    pub rollback_to: u64,
}

/// 回滚到 rollbackTo 版本 (先备份当前, 再从历史 zip 恢复)。
pub async fn rollback_version(
    resolver: &dyn WorkspaceResolver,
    config: &Config,
    ctx: &ProjectContext,
    code_version: &str,
    rollback_to: &str,
) -> AppResult<RollbackResult> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    let cur = parse_version(code_version)?;
    let to = parse_version(rollback_to)?;
    if to >= cur {
        return Err(AppError::validation(
            "rollbackTo must be less than codeVersion",
        ));
    }
    let project_path = resolver.resolve_project(ctx).await?;
    if !crate::service::fs_util::path_exists(&project_path).await? {
        return Err(AppError::resource("Project does not exist"));
    }
    let target_zip = version_zip_path(config, project_id, to);
    match verify_restore_source(&target_zip).await {
        RestoreSourceVerdict::Verified => {}
        RestoreSourceVerdict::Missing => {
            return Err(AppError::resource(format!(
                "Rollback version v{to} zip not found"
            )));
        }
        RestoreSourceVerdict::Corrupt(reason) => {
            // 目标损坏在清理项目之前拒绝 (P1-3): 原业务内容不动。
            return Err(AppError::resource(format!(
                "Rollback version v{to} zip is corrupt ({reason}); project left untouched"
            )));
        }
        RestoreSourceVerdict::InspectFailed(reason) => {
            return Err(AppError::system(format!(
                "inspect rollback version v{to} zip: {reason}"
            )));
        }
    }
    // 当前版本包缺失/损坏时重新备份 (原子发布保证重打不破坏旧文件);
    // 观察失败（权限等）如实上报, 不静默当缺失重打。
    let cur_zip = version_zip_path(config, project_id, cur);
    match verify_restore_source(&cur_zip).await {
        RestoreSourceVerdict::Verified => {}
        RestoreSourceVerdict::Missing | RestoreSourceVerdict::Corrupt(_) => {
            backup_project(config, project_id, &project_path, code_version).await?;
        }
        RestoreSourceVerdict::InspectFailed(reason) => {
            return Err(AppError::system(format!(
                "inspect current version v{cur} backup: {reason}"
            )));
        }
    }
    // 从目标版本恢复; 失败则尝试从刚才备份的当前版本恢复 (对齐 nuwax rollbackVersion catch)
    if let Err(e) = restore_from_zip(
        &project_path,
        &target_zip,
        &config.traverse_exclude_dirs,
        &config.backup_traverse_exclude_files,
    )
    .await
    {
        // 回退来源必须是完整核验通过的包, 不把"文件存在"当恢复能力 (FS-04)
        if matches!(
            verify_restore_source(&cur_zip).await,
            RestoreSourceVerdict::Verified
        ) {
            tracing::warn!(error = %e, "rollback restore failed, restoring current version backup");
            if let Err(restore_error) = restore_from_zip(
                &project_path,
                &cur_zip,
                &config.traverse_exclude_dirs,
                &config.backup_traverse_exclude_files,
            )
            .await
            {
                return Err(AppError::system(format!(
                    "rollback to v{to} failed: {e}; restoring current version v{cur} also failed: {restore_error}"
                )));
            }
        }
        return Err(e);
    }
    Ok(RollbackResult {
        new_version: cur,
        rollback_to: to,
    })
}

// ── get-project-content-by-version ──────────────────────────────────────────────

/// 解压历史版本 zip 到 `_his` 临时目录, 遍历后清理 (对齐 nuwax getProjectContentByVersion)。
pub async fn get_content_by_version(
    resolver: &dyn WorkspaceResolver,
    config: &Config,
    ctx: &ProjectContext,
    code_version: &str,
    proxy_path: Option<&str>,
    command: Option<&str>,
) -> AppResult<Vec<crate::service::tree::FileEntry>> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    let version = parse_version(code_version)?;
    let project_path = resolver.resolve_project(ctx).await?;
    let zip = version_zip_path(config, project_id, version);
    if !crate::service::fs_util::path_exists(&zip).await? {
        return Err(AppError::resource(format!(
            "Version v{version} zip not found"
        )));
    }
    // 每次请求使用独立临时目录，避免并发读取同一版本时互相清理。
    let history_root = match project_path.parent() {
        Some(parent) => parent.join("_his"),
        None => return Err(AppError::system("invalid project path")),
    };
    fs::create_dir_all(&history_root).await?;
    let history_temp = create_history_temp_dir(history_root).await?;
    let his_dir = history_temp.path().to_path_buf();

    // 解压与遍历结束后显式异步清理；TempDir 同时作为异常路径的 RAII 兜底。
    let mut files_result: AppResult<Vec<crate::service::tree::FileEntry>> = async {
        crate::service::zip::extract_to(zip, his_dir.clone()).await?;
        crate::service::tree::list_files(&his_dir, config, proxy_path).await
    }
    .await;
    let cleanup_result = fs::remove_dir_all(&his_dir).await;
    // command != "cpage_config" 时过滤 cpage_config.json (对齐 nuwax getContentUtils)
    if let Ok(files) = files_result.as_mut()
        && command != Some("cpage_config")
    {
        files.retain(|f| f.name != "cpage_config.json");
    }
    match (files_result, cleanup_result) {
        (Ok(files), Ok(())) => Ok(files),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(AppError::system(format!(
            "remove history temporary directory {}: {error}",
            his_dir.display()
        ))),
    }
}

async fn create_history_temp_dir(parent: PathBuf) -> AppResult<tempfile::TempDir> {
    tokio::task::spawn_blocking(move || {
        tempfile::Builder::new()
            .prefix("file-server-version-")
            .tempdir_in(&parent)
            .map_err(|error| {
                AppError::system(format!(
                    "create history temporary directory in {}: {error}",
                    parent.display()
                ))
            })
    })
    .await
    .map_err(|error| AppError::system(format!("history tempdir task failed: {error}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造"中央目录正常、payload 损坏"的 ZIP (P1-3 反例核心):
    /// 写一个 Stored（无压缩）entry 后, 直接翻转 local data 区一个字节——
    /// 中央目录/CRC 不动, `ZipArchive::new` 照常成功, 只有实际读取才 CRC 失败。
    fn corrupt_stored_payload(zip_path: &Path, data_marker: &[u8]) {
        let bytes = std::fs::read(zip_path).expect("read zip fixture");
        // local file header: PK\x03\x04 + 固定字段; data 紧随 name/extra 之后。
        // 找到 marker 在文件中的位置（Stored entry data 即原文, 唯一）。
        let position = bytes
            .windows(data_marker.len())
            .position(|window| window == data_marker)
            .expect("stored payload marker present");
        // 翻转 marker 之后的第 2 个字节: 保持 entry 名/头/中央目录/CRC 原样。
        let flip = position + data_marker.len() / 2 + 1;
        let mut corrupted = bytes;
        corrupted[flip] ^= 0xff;
        std::fs::write(zip_path, corrupted).expect("write corrupted fixture");
    }

    fn seed_project(dir: &Path) {
        std::fs::create_dir_all(dir.join("src")).expect("project dirs");
        std::fs::write(dir.join("src/app.js"), b"BUSINESS-SENTINEL").expect("sentinel");
        std::fs::write(dir.join("package.json"), b"{\"name\":\"app\"}").expect("manifest");
    }

    /// P1-3 反例: 目标版本包 payload 损坏（中央目录正常, 旧 `zip_usable` 会放行）
    /// → 回滚必须在**清理项目之前**拒绝; 原业务哨兵原样保留。
    #[cfg(unix)]
    #[tokio::test]
    async fn corrupt_payload_zip_is_rejected_before_clearing_project() {
        let root = tempfile::tempdir().expect("root");
        let project = root.path().join("proj");
        seed_project(&project);
        // 构造目标包: 含同名业务文件, data 区放 marker。
        let zip_path = root.path().join("proj-v1.zip");
        {
            use std::io::Write as _;
            let file = std::fs::File::create(&zip_path).expect("zip");
            let mut archive = zip::ZipWriter::new(file);
            archive
                .start_file(
                    "src/app.js",
                    zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Stored),
                )
                .expect("start stored entry");
            archive
                .write_all(b"OLD-VERSION-CONTENT-MARKER-0123456789")
                .expect("data");
            archive.finish().expect("finish");
        }
        corrupt_stored_payload(&zip_path, b"OLD-VERSION-CONTENT-MARKER-0123456789");
        // 中央目录仍可解析（旧判定的假阳性来源）。
        {
            let file = std::fs::File::open(&zip_path).unwrap();
            assert!(
                zip::ZipArchive::new(file).is_ok(),
                "fixture premise: central directory stays readable"
            );
        }

        let error = restore_from_zip(&project, &zip_path, &[], &[])
            .await
            .expect_err("corrupt payload must be rejected");
        let message = error.to_string();
        assert!(message.contains("corrupt"), "unexpected: {message}");
        assert!(
            message.contains("left untouched"),
            "rejection must state the project was not cleared: {message}"
        );
        // 项目未被清理: 业务哨兵与目录原样。
        assert_eq!(
            std::fs::read(project.join("src/app.js")).unwrap(),
            b"BUSINESS-SENTINEL",
            "business content must survive a rejected restore"
        );
    }

    /// P1-3: 目标完好 → 恢复成功（staging 落位, excluded 保留）。
    #[tokio::test]
    async fn verified_zip_restores_via_staging_keeping_excluded() {
        let root = tempfile::tempdir().expect("root");
        let project = root.path().join("proj");
        seed_project(&project);
        std::fs::create_dir_all(project.join("node_modules/pkg")).expect("excluded dir");
        std::fs::write(project.join("node_modules/pkg/x.js"), b"dep").expect("dep");

        let zip_path = root.path().join("proj-v1.zip");
        {
            use std::io::Write as _;
            let file = std::fs::File::create(&zip_path).expect("zip");
            let mut archive = zip::ZipWriter::new(file);
            archive
                .start_file("src/app.js", zip::write::SimpleFileOptions::default())
                .expect("entry");
            archive.write_all(b"RESTORED-CONTENT").expect("data");
            archive.finish().expect("finish");
        }

        restore_from_zip(&project, &zip_path, &["node_modules".to_string()], &[])
            .await
            .expect("verified restore succeeds");

        assert_eq!(
            std::fs::read(project.join("src/app.js")).unwrap(),
            b"RESTORED-CONTENT"
        );
        assert!(
            !project.join("package.json").exists(),
            "old entries not present in the restore zip must be cleared"
        );
        assert_eq!(
            std::fs::read(project.join("node_modules/pkg/x.js")).unwrap(),
            b"dep",
            "excluded directory kept"
        );
    }

    #[tokio::test]
    async fn clear_dir_keep_excluded_preserves_excluded_entries() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("fs_ver_{nanos}"));
        fs::create_dir_all(dir.join("node_modules")).await.unwrap();
        fs::create_dir_all(dir.join("src")).await.unwrap();
        fs::create_dir_all(dir.join(".git")).await.unwrap();
        fs::write(dir.join("src/app.js"), "x").await.unwrap();
        fs::write(dir.join("package-lock.yaml"), "x").await.unwrap();
        fs::write(dir.join("README.md"), "x").await.unwrap();

        clear_dir_keep_excluded(
            &dir,
            &["node_modules".into(), ".git".into()],
            &["package-lock.yaml".into()],
        )
        .await
        .unwrap();

        // excluded 保留 (node_modules / .git 目录 + lock 文件)
        assert!(dir.join("node_modules").exists());
        assert!(dir.join(".git").exists());
        assert!(dir.join("package-lock.yaml").exists());
        // 非 excluded 删除
        assert!(!dir.join("src").exists());
        assert!(!dir.join("README.md").exists());

        drop(fs::remove_dir_all(&dir).await);
    }

    #[test]
    fn parse_version_validates_number() {
        assert!(parse_version("abc").is_err());
        assert_eq!(parse_version("12").unwrap(), 12);
        assert_eq!(parse_version(" 3 ").unwrap(), 3);
    }
}

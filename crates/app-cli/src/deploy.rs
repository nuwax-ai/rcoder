//! URL 制品部署段（生产 Userapp 运行容器，RBD 卷形态）。
//!
//! rcoder `start {url}` 经 Deployment env 注入三元组：`APP_DEPLOY_URL`（制品 zip 地址）、
//! `APP_RELEASE_ID`（部署身份标识）、`APP_DEPLOY_SHA256`（可选校验，空 = 信任内网源）。
//! prepare downloads and validates into operation-owned temporary resources while
//! the old application continues serving. activate promotes prepared code only after
//! the caller stops the old processes. restore_previous restores code only; database
//! migration effects are never described as rolled back. RAII removes temporary
//! resources on completion, error, and cancellation, including blocking extraction.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::warn;

/// 部署状态 marker 文件名（卷根下，跨 code 换代存续）。
const DEPLOY_STATE_FILE: &str = ".deploy-state.toml";
/// 下载中转目录名（卷根下）。
const INCOMING_DIR: &str = ".incoming";
/// 解压 staging 目录名（卷根下）。
const STAGING_DIR: &str = ".staging";
/// 上一代 code 保留目录名（卷根下，仅一代）。
const PREVIOUS_DIR: &str = ".previous";

/// 部署状态（幂等 marker）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeployState {
    release_id: String,
    /// 实际下载内容的 sha256（hex，空 sha 部署时也有值——下载总会计算）。
    sha256: String,
    deployed_at: String,
}

/// 环境变量入口：`APP_DEPLOY_URL` 未设置/为空 → 跳过（走卷上已有 code 的兼容形态）。
pub async fn run_from_env(workspace: &Path) -> Result<()> {
    let Some(url) = env_non_empty("APP_DEPLOY_URL") else {
        return Ok(());
    };
    let Some(release_id) = env_non_empty("APP_RELEASE_ID") else {
        bail!("APP_DEPLOY_URL is set but APP_RELEASE_ID is missing — deploy env contract violated");
    };
    let sha256 = env_non_empty("APP_DEPLOY_SHA256").map(|s| s.to_ascii_lowercase());
    if let Some(sha) = &sha256
        && (sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        bail!("APP_DEPLOY_SHA256 must be 64 hex characters, got '{sha}'");
    }
    deploy(workspace, &url, &release_id, sha256.as_deref()).await
}

/// 是否请求了部署（main 用于决定是否先占位 liveness 端口）。
pub fn deploy_requested() -> bool {
    env_non_empty("APP_DEPLOY_URL").is_some()
}

/// 从 env 三元组解析部署请求（server 启动时的首次部署判定）。
/// 契约与 [`run_from_env`] 一致：URL 在而 RELEASE_ID 缺 = 契约违背（Err）。
pub fn request_from_env() -> Result<crate::server::DeployRequest> {
    let Some(url) = env_non_empty("APP_DEPLOY_URL") else {
        bail!("APP_DEPLOY_URL not set");
    };
    let Some(release_id) = env_non_empty("APP_RELEASE_ID") else {
        bail!("APP_DEPLOY_URL is set but APP_RELEASE_ID is missing — deploy env contract violated");
    };
    let sha256 = env_non_empty("APP_DEPLOY_SHA256").map(|s| s.to_ascii_lowercase());
    if let Some(sha) = &sha256
        && (sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        bail!("APP_DEPLOY_SHA256 must be 64 hex characters, got '{sha}'");
    }
    Ok(crate::server::DeployRequest {
        url,
        release_id,
        sha256,
    })
}

/// 部署期间的 liveness 端口托管。
///
/// 首次部署时 `/app/code` 尚不存在，api::serve（读 release.lock）无法启动，
/// :3010 无人应答 → kubelet liveness 在大制品下载窗口误杀容器。
/// 本托管在 deploy 前绑定 :3010：`/health` 200（进程活）、`/ready` 503
/// （未就绪摘流），deploy 结束（无论成败）后释放端口交还 api::serve。
pub struct LivenessHold {
    task: tokio::task::JoinHandle<()>,
}

impl LivenessHold {
    /// 绑定 admin 地址并开始应答探针。
    pub fn start(addr: &str) -> Result<Self> {
        let listener = std::net::TcpListener::bind(addr)
            .with_context(|| format!("bind liveness hold {addr}"))?;
        listener
            .set_nonblocking(true)
            .context("set liveness hold nonblocking")?;
        let listener = tokio::net::TcpListener::from_std(listener)
            .context("convert liveness hold listener")?;
        let task = tokio::spawn(async move {
            let app = axum::Router::new()
                .route(
                    "/health",
                    axum::routing::get(|| async {
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::json!({"status": "deploying"})),
                        )
                    }),
                )
                .route(
                    "/ready",
                    axum::routing::get(|| async {
                        (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            axum::Json(serde_json::json!({"status": "deploying"})),
                        )
                    }),
                )
                .fallback(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE });
            // 无限等待：task 被 abort（deploy 结束时）即整体退出
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { task })
    }

    /// 释放端口（abort serve task；listener 随 task 结束 drop）。
    pub async fn release(self) {
        self.task.abort();
        if let Err(e) = self.task.await
            && !e.is_cancelled()
        {
            warn!("liveness hold exited with error: {e}");
        }
    }
}

/// Prepared files are owned by this operation. Drop cleans failures/cancellation.
/// Extraction moves the owner into its blocking worker, so cancellation cannot race cleanup.
pub(crate) struct PreparedDeploy {
    staging: tempfile::TempDir,
    _lease: std::fs::File,
    state: DeployState,
}

pub(crate) async fn deploy(
    workspace: &Path,
    url: &str,
    release_id: &str,
    expected_sha: Option<&str>,
) -> Result<()> {
    if let Some(prepared) = prepare(workspace, url, release_id, expected_sha).await? {
        activate(workspace, prepared).await?;
    }
    Ok(())
}

pub(crate) async fn prepare(
    workspace: &Path,
    url: &str,
    release_id: &str,
    expected_sha: Option<&str>,
) -> Result<Option<PreparedDeploy>> {
    validate_release_id_fs_safe(release_id)?;
    let root = workspace.parent().context("workspace has no volume root")?;
    if let Some(state) = read_state(root).await
        && state.release_id == release_id
        && expected_sha.is_none_or(|sha| sha.eq_ignore_ascii_case(&state.sha256))
        && tokio::fs::try_exists(workspace.join("release.lock.toml")).await?
    {
        return Ok(None);
    }
    tokio::fs::create_dir_all(root)
        .await
        .context("create volume root")?;
    let lease = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".deploy-prepare.lock"))
        .context("open preparation lease")?;
    lease
        .try_lock()
        .context("another artifact preparation owns this volume")?;
    let incoming = incoming_dir(root);
    let staging_root = root.join(STAGING_DIR);
    tokio::fs::create_dir_all(&incoming)
        .await
        .context("create incoming directory")?;
    tokio::fs::create_dir_all(&staging_root)
        .await
        .context("create staging directory")?;
    // The OS lease outlives blocking extraction, including cancellation. Only a
    // holder can reclaim leftovers from a terminated operation.
    clean_owned_temporary(&incoming).await?;
    clean_owned_temporary(&staging_root).await?;
    let part = tempfile::Builder::new()
        .prefix("deploy-")
        .suffix(".part")
        .tempfile_in(&incoming)?;
    let actual_hex = to_hex(&download_to_file(url, part.path()).await?);
    if let Some(expected) = expected_sha
        && !expected.eq_ignore_ascii_case(&actual_hex)
    {
        bail!("artifact sha256 mismatch: expected {expected}, downloaded {actual_hex}");
    }
    verify_zip_magic(part.path()).await?;
    let staging = tempfile::Builder::new()
        .prefix("deploy-")
        .tempdir_in(staging_root)?;
    let (staging, lease) =
        tokio::task::spawn_blocking(move || -> Result<(tempfile::TempDir, std::fs::File)> {
            extract_zip_sync(part.path(), staging.path())?;
            crate::manifest::read_release_lock(staging.path())
                .context("staged package has no parsable release.lock.toml")?;
            Ok((staging, lease))
        })
        .await
        .context("join artifact preparation")??;
    Ok(Some(PreparedDeploy {
        staging,
        _lease: lease,
        state: DeployState {
            release_id: release_id.to_owned(),
            sha256: actual_hex,
            deployed_at: chrono::Utc::now().to_rfc3339(),
        },
    }))
}

/// Startup reclamation is exclusive with every live preparation, including
/// extraction that continues after its async caller has been cancelled.
pub(crate) async fn cleanup_startup(workspace: &Path) -> Result<()> {
    let root = workspace.parent().context("workspace has no volume root")?;
    tokio::fs::create_dir_all(root).await?;
    let lease = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".deploy-prepare.lock"))?;
    lease
        .try_lock()
        .context("active preparation prevents startup cleanup")?;
    for directory in [root.join(INCOMING_DIR), root.join(STAGING_DIR)] {
        if tokio::fs::try_exists(&directory).await? {
            clean_owned_temporary(&directory).await?;
        }
    }
    Ok(())
}

async fn clean_owned_temporary(directory: &Path) -> Result<()> {
    let mut entries = tokio::fs::read_dir(directory).await?;
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_name().to_string_lossy().starts_with("deploy-") {
            continue;
        }
        if entry.file_type().await?.is_dir() {
            tokio::fs::remove_dir_all(entry.path()).await?;
        } else {
            tokio::fs::remove_file(entry.path()).await?;
        }
    }
    Ok(())
}

/// Activate only after the caller has stopped the previous generation.
#[derive(Debug)]
pub(crate) struct ActivationFailure {
    pub error: anyhow::Error,
    pub code_preserved: bool,
}
impl std::fmt::Display for ActivationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.error)
    }
}
impl std::error::Error for ActivationFailure {}

pub(crate) async fn activate(
    workspace: &Path,
    prepared: PreparedDeploy,
) -> Result<(), ActivationFailure> {
    let mut code_preserved = true;
    activate_inner(workspace, prepared, &mut code_preserved)
        .await
        .map_err(|error| ActivationFailure {
            error,
            code_preserved,
        })
}

async fn activate_inner(
    workspace: &Path,
    prepared: PreparedDeploy,
    code_preserved: &mut bool,
) -> Result<()> {
    let root = workspace.parent().context("workspace has no volume root")?;
    let previous = root.join(PREVIOUS_DIR);
    if tokio::fs::try_exists(&previous).await? {
        tokio::fs::remove_dir_all(&previous)
            .await
            .context("clean previous generation")?;
    }
    let had_previous = tokio::fs::try_exists(workspace).await?;
    if had_previous {
        tokio::fs::rename(workspace, &previous)
            .await
            .context("preserve previous generation")?;
        *code_preserved = false;
    }
    if let Err(error) = tokio::fs::rename(prepared.staging.path(), workspace).await {
        if had_previous {
            tokio::fs::rename(&previous, workspace)
                .await
                .context("restore previous after failed promotion")?;
            *code_preserved = true;
        }
        return Err(error).context("promote prepared generation");
    }
    *code_preserved = false;
    let marker = tempfile::NamedTempFile::new_in(root)?;
    tokio::fs::write(marker.path(), toml::to_string_pretty(&prepared.state)?).await?;
    tokio::fs::rename(marker.path(), root.join(DEPLOY_STATE_FILE))
        .await
        .context("commit deployment marker")?;
    Ok(())
}

/// Restore code only. Database migrations are deliberately not reversed.
pub(crate) async fn restore_previous(workspace: &Path) -> Result<bool> {
    let root = workspace.parent().context("workspace has no volume root")?;
    let previous = root.join(PREVIOUS_DIR);
    if !tokio::fs::try_exists(previous.join("release.lock.toml")).await? {
        return Ok(false);
    }
    if tokio::fs::try_exists(workspace).await? {
        tokio::fs::remove_dir_all(workspace)
            .await
            .context("remove failed code generation")?;
    }
    tokio::fs::rename(previous, workspace)
        .await
        .context("restore previous code generation")?;
    let marker = root.join(DEPLOY_STATE_FILE);
    if tokio::fs::try_exists(&marker).await? {
        tokio::fs::remove_file(marker).await?;
    }
    Ok(true)
}

/// 下载中转目录路径（卷根下）。
fn incoming_dir(volume_root: &Path) -> std::path::PathBuf {
    volume_root.join(INCOMING_DIR)
}

/// 清 `.incoming/` 下全部 `*.part` 残片（deploy stage 入口调用）。
///
/// 跨重启遗留：成功路径自清、sha mismatch 清、其余失败路径由 staged 块兜底清，
/// 但历史版本残片 / 进程被 SIGKILL 的窗口仍可能留档——下载前统一扫掉，
/// 对齐 file-server 侧 hygiene sweep 的模型。失败仅 warn（清理是卫生动作，
/// 不阻断部署主流程）。
#[cfg(test)]
async fn sweep_incoming_parts(dir: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return; // 目录不存在 = 无残片
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "part")
            && let Err(e) = tokio::fs::remove_file(&path).await
        {
            warn!("sweep stale part {} failed: {e}", path.display());
        }
    }
}

/// 制品 zip 魔数校验（下载完整落盘后读文件头，非流式截取）。
///
/// 拦截形态：下载 URL 返回 **HTTP 200 + 错误信封 body**（如网关把 401/404 包成
/// 200 + JSON code 信封返回）——下载器在 HTTP 层无感知，错误 body 被当 zip 落盘，
/// 直到解压层才报深层 "Could not find EOCD"（线上事故形态）。此处前置拦截并给
/// 出带首字节线索的可诊断错误。
///
/// 判据：ZIP 规范中含 ≥1 条目的 zip 偏移 0 必是第一个条目的 Local File Header
/// （PK\x03\x04），与生成工具无关；`infer::archive::is_zip` 还额外放行空 zip /
/// 跨卷等边缘形态（平台制品必含 release.lock.toml，空包由 lock 闸门拒绝，跨卷
/// zip crate 本就不支持——宽松方向只会放行"真 zip"，无误伤）。body 不足 4 字节
/// （0 字导体/极短错误页）is_zip 恒 false，天然归入同一错误。
async fn verify_zip_magic(part: &Path) -> Result<()> {
    let mut file = tokio::fs::File::open(part)
        .await
        .with_context(|| format!("open downloaded part {}", part.display()))?;
    let mut head = [0u8; 4];
    let n = file
        .read(&mut head)
        .await
        .with_context(|| format!("read head of {}", part.display()))?;
    let head = &head[..n];
    if !infer::archive::is_zip(head) {
        bail!(
            "downloaded artifact is not a zip archive (first bytes: {}) — \
             the url likely returned an error envelope with HTTP 200; \
             verify the deploy url and its auth (release_id in path)",
            head.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
    }
    Ok(())
}

/// 流式下载到文件，返回内容 sha256。无整体超时与容量限制；保留连接和读取空闲超时。
async fn download_to_file(url: &str, dest: &Path) -> Result<[u8; 32]> {
    let idle = Duration::from_secs(positive_setting("APP_DEPLOY_READ_IDLE_SECONDS", 60)?);
    let client = reqwest::Client::builder()
        .read_timeout(idle)
        .connect_timeout(Duration::from_secs(15))
        .build()
        .context("build deploy http client")?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))
        .and_then(|r| r.error_for_status().with_context(|| format!("GET {url}")))?;
    let mut stream = response.bytes_stream();
    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("create {}", dest.display()))?;
    let mut hasher = Sha256::new();
    use futures::StreamExt;
    while let Some(chunk) = tokio::time::timeout(idle, stream.next())
        .await
        .context("download read idle timeout")?
    {
        let chunk = chunk.context("download stream")?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&hasher.finalize());
    Ok(out)
}

fn positive_setting(key: &str, default: u64) -> Result<u64> {
    match std::env::var(key) {
        Ok(value) => {
            let n: u64 = value.parse().with_context(|| format!("invalid {key}"))?;
            if n == 0 {
                bail!("{key} must be positive");
            }
            Ok(n)
        }
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(e).with_context(|| format!("read {key}")),
    }
}

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

async fn read_state(volume_root: &Path) -> Option<DeployState> {
    let path = volume_root.join(DEPLOY_STATE_FILE);
    let content = tokio::fs::read_to_string(&path).await.ok()?;
    match toml::from_str(&content) {
        Ok(state) => Some(state),
        Err(e) => {
            warn!(
                "parse {} failed ({e}); treating as no marker",
                path.display()
            );
            None
        }
    }
}

/// release_id 进 fs 路径（.incoming/.staging）与 env，白名单收紧。
fn validate_release_id_fs_safe(release_id: &str) -> Result<()> {
    let ok = !release_id.is_empty()
        && !release_id.starts_with('.')
        && release_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if !ok {
        bail!("APP_RELEASE_ID must be [A-Za-z0-9._-]+ (no leading dot), got '{release_id}'");
    }
    Ok(())
}

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// volume_root 辅助（测试共用）：workspace = {vol}/code。
#[cfg(test)]
fn volume_root_of(workspace: &Path) -> std::path::PathBuf {
    workspace.parent().expect("workspace parent").to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 workspace-manifest golden fixture 同形状的最小可解析 lock。
    const MINIMAL_LOCK: &str = r#"
schema_version = 1
release_id = "test-release-0001"
workspace_name = "demo"
minimum_app_cli_version = "0.1.3"
runtime_image_digest = "registry.example/app-runtime:0.1.140"

[pingap]
mode = "managed"
version = "0.14.1"
commit = "abc123"

[[services]]
service_id = "web"
name = "Web"
dir = "web"
type = "node"
kind = "web"
enabled = true
port = 4200

[services.run]
command = ["node", "server.js"]
migrate = []
depends_on = []
shutdown_timeout_seconds = 30

[services.health]
startup_path = "/health"
readiness_path = "/ready"
liveness_path = "/health"

[[services.logs]]
id = "application"
glob = "web*.log*"
format = "jsonl"

[services.env]
"#;

    #[tokio::test]
    async fn preparation_lease_protects_active_staging_and_reclaims_stale_owned_files() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("code");
        let incoming = root.path().join(INCOMING_DIR);
        tokio::fs::create_dir_all(&incoming)
            .await
            .expect("incoming");
        tokio::fs::write(incoming.join("deploy-stale.part"), "stale")
            .await
            .expect("stale");
        tokio::fs::write(incoming.join("unrelated"), "keep")
            .await
            .expect("unrelated");
        let url = serve_once(build_zip(&[("release.lock.toml", MINIMAL_LOCK)])).await;
        let prepared = prepare(&workspace, &url, "prepared", None)
            .await
            .expect("prepare")
            .expect("new");
        let staging = prepared.staging.path().to_owned();
        assert!(!incoming.join("deploy-stale.part").exists());
        assert!(incoming.join("unrelated").exists());
        assert!(
            prepare(&workspace, "http://127.0.0.1:1/unused", "contender", None)
                .await
                .is_err()
        );
        assert!(
            cleanup_startup(&workspace).await.is_err(),
            "startup sweep must respect the live lease"
        );
        assert!(staging.exists(), "contender must not clean active staging");
        drop(prepared);
        assert!(!staging.exists());
    }

    #[tokio::test]
    async fn preparation_does_not_replace_serving_code_and_drop_cleans_staging() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("code");
        tokio::fs::create_dir_all(&workspace).await.expect("mkdir");
        tokio::fs::write(workspace.join("old.txt"), "old")
            .await
            .expect("old");
        let url = serve_once(build_zip(&[
            ("release.lock.toml", MINIMAL_LOCK),
            ("new.txt", "new"),
        ]))
        .await;
        let prepared = prepare(&workspace, &url, "new-release", None)
            .await
            .expect("prepare")
            .expect("prepared");
        assert_eq!(
            tokio::fs::read_to_string(workspace.join("old.txt"))
                .await
                .expect("old"),
            "old"
        );
        assert!(!workspace.join("new.txt").exists());
        let staging = prepared.staging.path().to_path_buf();
        assert!(staging.exists());
        drop(prepared);
        assert!(!staging.exists());
        assert_eq!(
            std::fs::read_dir(incoming_dir(root.path()))
                .expect("incoming")
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn failed_manifest_cleans_staging_and_download() {
        let root = tempfile::tempdir().expect("root");
        let url = serve_once(build_zip(&[("untrusted.txt", "payload")])).await;
        assert!(
            prepare(&root.path().join("code"), &url, "bad", None)
                .await
                .is_err()
        );
        for name in [INCOMING_DIR, STAGING_DIR] {
            assert_eq!(
                std::fs::read_dir(root.path().join(name))
                    .expect("directory")
                    .count(),
                0
            );
        }
    }

    #[test]
    fn extraction_preserves_content_without_quota_configuration() {
        let root = tempfile::tempdir().expect("root");
        let archive = root.path().join("input.zip");
        let content = "x".repeat(20000);
        let names: Vec<_> = (0..16).map(|i| format!("entry-{i}")).collect();
        let entries: Vec<_> = names
            .iter()
            .map(|name| (name.as_str(), content.as_str()))
            .collect();
        std::fs::write(&archive, build_zip(&entries)).expect("zip");
        let out = tempfile::tempdir().expect("out");
        extract_zip_sync(&archive, out.path()).expect("extract without quota configuration");
        for name in names {
            assert_eq!(
                std::fs::read_to_string(out.path().join(name)).expect("content"),
                content
            );
        }
    }

    fn build_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut buf);
        for (name, content) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .expect("start file");
            std::io::Write::write_all(&mut zip, content.as_bytes()).expect("write entry");
        }
        zip.finish().expect("finish zip");
        buf.into_inner()
    }

    /// 极简本地 HTTP 服务：单次请求返回 body（Connection: close）。
    async fn serve_once(body: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 4096];
            // 读到请求头结束即可（GET 无 body）
            loop {
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 || buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket
                .write_all(header.as_bytes())
                .await
                .expect("write head");
            socket.write_all(&body).await.expect("write body");
        });
        format!("http://{addr}/artifact.zip")
    }

    fn make_volume() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("code");
        std::fs::create_dir_all(&workspace).expect("mkdir code");
        (dir, workspace)
    }

    #[tokio::test]
    async fn fresh_deploy_extracts_and_writes_marker() {
        let (_dir, workspace) = make_volume();
        let zip_bytes = build_zip(&[
            ("release.lock.toml", MINIMAL_LOCK),
            ("web/server.js", "console.log('hi')"),
        ]);
        let url = serve_once(zip_bytes.clone()).await;
        let sha = {
            let mut h = Sha256::new();
            h.update(&zip_bytes);
            to_hex(&h.finalize())
        };

        deploy(&workspace, &url, "rel-001", Some(&sha))
            .await
            .expect("deploy");

        assert!(workspace.join("release.lock.toml").exists());
        assert!(workspace.join("web/server.js").exists());
        let state = read_state(&volume_root_of(&workspace))
            .await
            .expect("state written");
        assert_eq!(state.release_id, "rel-001");
        assert_eq!(state.sha256, sha);
        // .part 已清理
        assert!(
            !volume_root_of(&workspace)
                .join(INCOMING_DIR)
                .join("rel-001.zip.part")
                .exists()
        );
    }

    #[tokio::test]
    async fn marker_hit_skips_download() {
        let (_dir, workspace) = make_volume();
        let zip_bytes = build_zip(&[("release.lock.toml", MINIMAL_LOCK)]);
        let url = serve_once(zip_bytes).await;
        deploy(&workspace, &url, "rel-001", None)
            .await
            .expect("first deploy");

        // 第二次：URL 指向必然失败的地址（连接拒绝端口），marker 命中应跳过下载
        let bad_url = "http://127.0.0.1:1/nothing.zip";
        deploy(&workspace, bad_url, "rel-001", None)
            .await
            .expect("marker skip");
        // code 仍是第一次部署的内容
        assert!(workspace.join("release.lock.toml").exists());
    }

    #[tokio::test]
    async fn sha_mismatch_fails_and_preserves_code() {
        let (_dir, workspace) = make_volume();
        // 预置现场（旧 code）
        std::fs::write(workspace.join("sentinel.txt"), "old").expect("sentinel");

        let zip_bytes = build_zip(&[("release.lock.toml", MINIMAL_LOCK)]);
        let url = serve_once(zip_bytes).await;
        let err = deploy(&workspace, &url, "rel-001", Some(&"0".repeat(64)))
            .await
            .expect_err("mismatch must fail");
        assert!(err.to_string().contains("sha256 mismatch"));
        // 现场 code 未被破坏
        assert_eq!(
            std::fs::read_to_string(workspace.join("sentinel.txt")).unwrap(),
            "old"
        );
    }

    #[tokio::test]
    async fn zip_slip_entry_rejected() {
        let (_dir, workspace) = make_volume();
        // enclosed_name 对 "../evil" 返回 None → 解压即拒
        let zip_bytes = build_zip(&[("../evil.txt", "boom")]);
        let url = serve_once(zip_bytes).await;
        let err = deploy(&workspace, &url, "rel-001", None)
            .await
            .expect_err("zip-slip must fail");
        // anyhow 链式上下文：{:#} 展开整链（to_string 只显最外层 context）
        let chain = format!("{err:#}");
        assert!(
            chain.contains("zip-slip") || chain.contains("escapes"),
            "unexpected error: {chain}"
        );
        assert!(!volume_root_of(&workspace).join("evil.txt").exists());
    }

    #[tokio::test]
    async fn missing_lock_in_package_rejected() {
        let (_dir, workspace) = make_volume();
        let zip_bytes = build_zip(&[("random.txt", "not a platform artifact")]);
        let url = serve_once(zip_bytes).await;
        let err = deploy(&workspace, &url, "rel-001", None)
            .await
            .expect_err("package without lock must fail");
        assert!(err.to_string().contains("release.lock.toml"));
        // lock 闸门失败也清 .part（staged 块统一清理，不留残片）
        assert!(
            !volume_root_of(&workspace)
                .join(INCOMING_DIR)
                .join("rel-001.zip.part")
                .exists()
        );
    }

    /// 线上事故形态锁：URL 返回 HTTP 200 + JSON 错误信封（网关把 401 包成 200），
    /// 下载器无感知落盘——必须在魔数校验层拦截并给出可诊断错误，而非深层的
    /// "Could not find EOCD"。
    #[tokio::test]
    async fn deploy_rejects_non_zip_body() {
        let (_dir, workspace) = make_volume();
        std::fs::write(workspace.join("sentinel.txt"), "old").expect("sentinel");
        let envelope = br#"{"code":"4010","message":"not logged in","success":false}"#;
        let url = serve_once(envelope.to_vec()).await;
        let err = deploy(&workspace, &url, "rel-001", None)
            .await
            .expect_err("error envelope must be rejected");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("not a zip archive"),
            "unexpected error: {chain}"
        );
        // 首字节线索：7b22636f = `{"co`（JSON 信封指纹）
        assert!(chain.contains("7b22636f"), "missing head hex: {chain}");
        // .part 已清 + 现场 code 未破坏
        assert!(
            !volume_root_of(&workspace)
                .join(INCOMING_DIR)
                .join("rel-001.zip.part")
                .exists()
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("sentinel.txt")).unwrap(),
            "old"
        );
    }

    /// 0 字节 body（某些网关错误页）：读不满 4 字节同样归入 not-a-zip 拒绝。
    #[tokio::test]
    async fn deploy_rejects_empty_body() {
        let (_dir, workspace) = make_volume();
        let url = serve_once(Vec::new()).await;
        let err = deploy(&workspace, &url, "rel-001", None)
            .await
            .expect_err("empty body must be rejected");
        assert!(
            format!("{err:#}").contains("not a zip archive"),
            "unexpected error: {err:#}"
        );
        assert!(
            !volume_root_of(&workspace)
                .join(INCOMING_DIR)
                .join("rel-001.zip.part")
                .exists()
        );
    }

    /// 启动 sweep：跨重启遗留的 *.part 残片在下一次 deploy 入口被清掉，
    /// 非 .part 文件（.staging 中转等）不受影响。
    #[tokio::test]
    async fn sweep_clears_stale_parts_only() {
        let (_dir, workspace) = make_volume();
        let volume_root = volume_root_of(&workspace);
        let incoming = volume_root.join(INCOMING_DIR);
        std::fs::create_dir_all(&incoming).expect("mkdir incoming");
        std::fs::write(incoming.join("old-release.zip.part"), b"stale").expect("stale part");
        std::fs::write(incoming.join("keep.txt"), b"not a part").expect("keep file");

        sweep_incoming_parts(&incoming).await;

        assert!(!incoming.join("old-release.zip.part").exists());
        assert!(incoming.join("keep.txt").exists());
    }

    #[tokio::test]
    async fn second_deploy_preserves_previous_generation() {
        let (_dir, workspace) = make_volume();
        let zip_v1 = build_zip(&[("release.lock.toml", MINIMAL_LOCK), ("v.txt", "1")]);
        let url1 = serve_once(zip_v1).await;
        deploy(&workspace, &url1, "rel-001", None)
            .await
            .expect("v1");

        let lock_v2 = MINIMAL_LOCK.replace("test-release-0001", "test-release-0002");
        let zip_v2 = build_zip(&[("release.lock.toml", &lock_v2), ("v.txt", "2")]);
        let url2 = serve_once(zip_v2).await;
        deploy(&workspace, &url2, "rel-002", None)
            .await
            .expect("v2");

        assert_eq!(
            std::fs::read_to_string(workspace.join("v.txt")).unwrap(),
            "2"
        );
        let previous = volume_root_of(&workspace).join(PREVIOUS_DIR);
        assert_eq!(
            std::fs::read_to_string(previous.join("v.txt")).unwrap(),
            "1"
        );
    }

    #[test]
    fn release_id_fs_safe_validation() {
        assert!(validate_release_id_fs_safe("rel-abc_1.2").is_ok());
        assert!(validate_release_id_fs_safe("../evil").is_err());
        assert!(validate_release_id_fs_safe(".hidden").is_err());
        assert!(validate_release_id_fs_safe("a/b").is_err());
        assert!(validate_release_id_fs_safe("").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn deploy_preserves_internal_dependency_symlink() {
        use std::io::Write;
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("artifact.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).expect("zip"));
        zip.start_file("lib/index.js", zip::write::SimpleFileOptions::default())
            .expect("file");
        zip.write_all(b"module.exports = 42").expect("content");
        zip.add_symlink(
            "node_modules/next",
            "../lib",
            zip::write::SimpleFileOptions::default(),
        )
        .expect("link");
        zip.finish().expect("finish");
        let out = root.path().join("out");
        std::fs::create_dir_all(&out).expect("out");
        extract_zip_sync(&path, &out).expect("extract");
        assert!(out.join("node_modules/next").is_symlink());
        assert_eq!(
            std::fs::read_to_string(out.join("node_modules/next/index.js"))
                .expect("read linked dependency"),
            "module.exports = 42"
        );
    }
}

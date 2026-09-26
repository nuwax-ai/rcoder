//! UserApp 错误页存储/加载服务（rcoder-engine 装配；rcoder-proxy 与 Axum
//! 管理面共享同一实例）。
//!
//! 模块边界（proxy-error-page.md §6）：本服务持有唯一权威与唯一缓存发布路径
//! ——上传处理器不把请求 body 直接塞缓存，而是保存到权威存储后触发同一加载
//! 器读回，避免"先新后旧"覆盖；错误请求/管理 GET 触发按需刷新（5s 节流、
//! 单刷新执行者），无请求时不做后台轮询。
//!
//! 存储后端由 **RCoder 自身部署形态** 决定（与管理 Docker 还是 K8s 无关）：
//! 宿主机/Compose → file（本地持久化目录）；Pod 内 → kubernetes-configmap
//! （API 写固定 ConfigMap，读目录投射）。
//!
//! 失败语义：页面读取/校验失败保留最近有效页或内置页，不拖延错误响应、
//! 不影响探针与启停；存储未配置/写入失败明确报错，不写临时层假成功。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arc_swap::ArcSwapOption;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use rcoder_proxy::error_page::{ErrorPageRenderer, ErrorPageSnapshot};

/// 单文件上限（512 KiB；按实际字节数限制）。
pub const MAX_PAGE_BYTES: usize = 512 * 1024;
/// ConfigMap 单对象数据上限（K8s 限制 1 MiB；含其他 key 总量）。
#[cfg(feature = "kubernetes")]
const CONFIGMAP_MAX_BYTES: usize = 1024 * 1024;
/// 按需刷新节流间隔（非 SLA——无请求的副本可以延迟加载）。
const REFRESH_THROTTLE: Duration = Duration::from_secs(5);
/// 文件读取预算（网络卷/坏文件不能拖住故障响应或管理面）。
const READ_BUDGET: Duration = Duration::from_secs(2);

/// 页面存储后端选择。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorPageBackend {
    /// 本地持久化文件（宿主机/Compose；宿主机管理 K8s 也用本地文件）
    File,
    /// K8s ConfigMap（Pod 内部署：API 写固定对象，读目录投射）
    KubernetesConfigmap,
}

/// UserApp 错误页配置。
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserAppErrorPageConfig {
    pub backend: ErrorPageBackend,
    /// file 后端：权威文件路径（上传原子替换到同目录；读取同文件）。
    /// env `RCODER_USERAPP_ERROR_PAGE_FILE` 覆盖。
    #[serde(default)]
    pub file: Option<PathBuf>,
    /// configmap 后端：ConfigMap 名称（固定对象；缺失时创建）。
    #[serde(default)]
    pub configmap_name: Option<String>,
    /// configmap 后端：namespace（Pod 内默认当前 namespace）。
    #[serde(default)]
    pub configmap_namespace: Option<String>,
    /// configmap 后端：页面 key（默认 `userapp-error.html`；只操作该 key）。
    #[serde(default)]
    pub configmap_key: Option<String>,
    /// configmap 后端：目录投射路径（读取来源；默认 `/etc/rcoder/error-pages`）。
    #[serde(default)]
    pub mounted_dir: Option<PathBuf>,
}

impl UserAppErrorPageConfig {
    /// 从配置 + env 解析出有效路径/名称（Fail Fast：后端必需项缺失报错）。
    fn resolve(&self) -> Result<ResolvedConfig> {
        match self.backend {
            ErrorPageBackend::File => {
                let path = std::env::var_os("RCODER_USERAPP_ERROR_PAGE_FILE")
                    .map(PathBuf::from)
                    .or_else(|| self.file.clone())
                    .context("error page backend=file requires a file path (config `file` or env RCODER_USERAPP_ERROR_PAGE_FILE)")?;
                #[cfg(feature = "kubernetes")]
                {
                    Ok(ResolvedConfig {
                        backend: ErrorPageBackend::File,
                        authoritative_path: path.clone(),
                        read_path: path,
                        configmap_name: None,
                        configmap_namespace: None,
                        configmap_key: None,
                    })
                }
                #[cfg(not(feature = "kubernetes"))]
                {
                    Ok(ResolvedConfig {
                        backend: ErrorPageBackend::File,
                        authoritative_path: path.clone(),
                        read_path: path,
                    })
                }
            }
            ErrorPageBackend::KubernetesConfigmap => {
                #[cfg(feature = "kubernetes")]
                {
                    let name = self.configmap_name.clone().context(
                        "error page backend=kubernetes-configmap requires configmap_name",
                    )?;
                    let namespace = self
                        .configmap_namespace
                        .clone()
                        .or_else(|| std::env::var("RCODER_K8S_NAMESPACE").ok())
                        .context("error page backend=kubernetes-configmap requires configmap_namespace (or env RCODER_K8S_NAMESPACE)")?;
                    let key = self
                        .configmap_key
                        .clone()
                        .unwrap_or_else(|| "userapp-error.html".to_string());
                    let dir = self
                        .mounted_dir
                        .clone()
                        .unwrap_or_else(|| PathBuf::from("/etc/rcoder/error-pages"));
                    Ok(ResolvedConfig {
                        backend: self.backend.clone(),
                        authoritative_path: dir.join(&key),
                        read_path: dir.join(&key),
                        configmap_name: Some(name),
                        configmap_namespace: Some(namespace),
                        configmap_key: Some(key),
                    })
                }
                #[cfg(not(feature = "kubernetes"))]
                {
                    // 无 kube 客户端的构建：装配即报配置错误（fail fast），
                    // 不静默降级到别的后端。
                    anyhow::bail!(
                        "error page backend=kubernetes-configmap requires a build with the kubernetes feature"
                    );
                }
            }
        }
    }
}

pub(crate) struct ResolvedConfig {
    pub(crate) backend: ErrorPageBackend,
    /// file 后端 = 权威文件；configmap 后端 = 投射文件（读取面）。
    pub(crate) authoritative_path: PathBuf,
    pub(crate) read_path: PathBuf,
    #[cfg(feature = "kubernetes")]
    pub(crate) configmap_name: Option<String>,
    #[cfg(feature = "kubernetes")]
    pub(crate) configmap_namespace: Option<String>,
    #[cfg(feature = "kubernetes")]
    pub(crate) configmap_key: Option<String>,
}

/// 本副本加载状态（管理 GET 透出；区分"已保存"与"本副本已加载"）。
#[derive(Debug, Clone, Default)]
struct LoadState {
    loaded_sha256: Option<String>,
    loaded_source: Option<&'static str>,
    loaded_at: Option<String>,
    last_checked_at: Option<String>,
    last_load_error_code: Option<String>,
}

/// 权威存储摘要（管理 GET 透出）。
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct ErrorPageStoredSummary {
    /// 权威覆盖内容摘要；None = 无覆盖（内置页生效）
    pub stored_sha256: Option<String>,
}

/// 管理 GET 的完整状态响应。
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct ErrorPageAdminStatus {
    /// 本进程实例标识（多副本区分用；每次启动重新生成）
    pub instance_id: String,
    /// 权威存储摘要
    pub stored: ErrorPageStoredSummary,
    /// 本副本已加载的页面摘要
    pub loaded_sha256: Option<String>,
    /// `builtin` / `external`
    pub loaded_source: Option<String>,
    /// 本副本加载完成时间（RFC3339；缺可靠来源为 null）
    pub loaded_at: Option<String>,
    /// 最近一次缓存检查时间
    pub last_checked_at: Option<String>,
    /// 最近一次加载错误码（无错误为 null）
    pub last_load_error_code: Option<String>,
    /// 本副本缓存是否与权威一致（权威无覆盖且本副本用内置页 → true）
    pub in_sync: bool,
    /// 生效后端
    pub backend: String,
}

/// 上传/删除操作的确认结果（保存成功 ≠ 所有副本已加载）。
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct ErrorPageWriteResult {
    /// 权威存储当前覆盖摘要（delete 后恢复内置 → None）
    pub stored_sha256: Option<String>,
    /// 本副本当前已加载摘要（可能与 stored 不同——投射/缓存传播延迟）
    pub loaded_sha256: Option<String>,
    /// 操作后本副本是否已与权威一致
    pub loaded_by_this_replica: bool,
}

/// 存储操作失败（结构化错误码 + 上下文）。
#[derive(Debug, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ErrorPageStoreError {
    pub code: &'static str,
    pub message: String,
}

impl ErrorPageStoreError {
    fn invalid(message: String) -> Self {
        Self {
            code: "ERROR_PAGE_INVALID",
            message,
        }
    }

    fn io(message: String) -> Self {
        Self {
            code: "ERROR_PAGE_STORE_IO",
            message,
        }
    }

    #[cfg(feature = "kubernetes")]
    fn conflict(message: String) -> Self {
        Self {
            code: "ERROR_PAGE_STORE_CONFLICT",
            message,
        }
    }
}

/// UserApp 错误页服务（唯一权威 + 唯一缓存发布路径）。
pub struct UserAppErrorPageService {
    config: ResolvedConfig,
    instance_id: String,
    cache: ArcSwapOption<ErrorPageSnapshot>,
    state: std::sync::Mutex<LoadState>,
    /// 单刷新执行者（并发请求合并；IO 只在锁内做、锁只属于刷新）。
    refresh_gate: Mutex<()>,
    last_refresh: Mutex<Option<std::time::Instant>>,
}

impl UserAppErrorPageService {
    /// 装配（配置解析失败 = 配置错误，fail fast 由调用方决定是否阻断启动）。
    pub fn new(config: &UserAppErrorPageConfig) -> Result<Self> {
        let resolved = config.resolve()?;
        Ok(Self {
            config: resolved,
            instance_id: uuid::Uuid::new_v4().simple().to_string(),
            cache: ArcSwapOption::from(None),
            state: std::sync::Mutex::new(LoadState::default()),
            refresh_gate: Mutex::new(()),
            last_refresh: Mutex::new(None),
        })
    }

    /// 呈现器（与 Pingora/Axum 共享本实例——`ErrorPageSource` 实现读取缓存）。
    pub fn renderer(self: &Arc<Self>) -> Arc<ErrorPageRenderer> {
        Arc::new(ErrorPageRenderer::new(Some(self.clone())))
    }

    /// 管理 GET：状态（触发一次按需刷新检查；读权威摘要不冒充缓存值）。
    pub async fn admin_status(&self) -> Result<ErrorPageAdminStatus, ErrorPageStoreError> {
        self.refresh_if_stale().await;
        let stored_sha = self.stored_sha256().await?;
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let loaded_sha = state.loaded_sha256.clone();
        let in_sync = match (&stored_sha, &loaded_sha) {
            (None, None) => true,
            (Some(stored), Some(loaded)) => stored == loaded,
            _ => false,
        };
        Ok(ErrorPageAdminStatus {
            instance_id: self.instance_id.clone(),
            stored: ErrorPageStoredSummary {
                stored_sha256: stored_sha,
            },
            loaded_sha256: loaded_sha,
            loaded_source: state.loaded_source.map(str::to_string),
            loaded_at: state.loaded_at.clone(),
            last_checked_at: state.last_checked_at.clone(),
            last_load_error_code: state.last_load_error_code.clone(),
            in_sync,
            backend: backend_name(&self.config.backend).to_string(),
        })
    }

    /// 上传保存：完整校验 → 权威原子替换 → 同一加载器读回发布缓存。
    pub async fn save(&self, content: &[u8]) -> Result<ErrorPageWriteResult, ErrorPageStoreError> {
        validate_page(content).map_err(ErrorPageStoreError::invalid)?;
        let sha = sha256_hex(content);
        match self.config.backend {
            ErrorPageBackend::File => self.save_file(content).await?,
            ErrorPageBackend::KubernetesConfigmap => self.save_configmap(content).await?,
        }
        // 保存成功后强制刷新（不经节流——上传路径本就是低频管理操作）。
        self.load_from_read_path().await;
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let loaded_by_this_replica = state.loaded_sha256.as_deref() == Some(sha.as_str());
        Ok(ErrorPageWriteResult {
            stored_sha256: Some(sha),
            loaded_sha256: state.loaded_sha256.clone(),
            loaded_by_this_replica,
        })
    }

    /// 恢复默认：移除权威覆盖（幂等；K8s 只删页面 key，保留其他内容）。
    pub async fn delete(&self) -> Result<ErrorPageWriteResult, ErrorPageStoreError> {
        match self.config.backend {
            ErrorPageBackend::File => {
                self.delete_file().await?;
            }
            ErrorPageBackend::KubernetesConfigmap => {
                self.delete_configmap_key().await?;
            }
        }
        self.load_from_read_path().await;
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // file 后端删除即权威消失；configmap 投射存在传播延迟——以权威读取为准。
        let stored = self.stored_sha256().await.ok().flatten();
        Ok(ErrorPageWriteResult {
            stored_sha256: stored,
            loaded_sha256: state.loaded_sha256.clone(),
            loaded_by_this_replica: state.loaded_sha256.is_none(),
        })
    }

    async fn save_file(&self, content: &[u8]) -> Result<(), ErrorPageStoreError> {
        let path = &self.config.authoritative_path;
        let directory = path
            .parent()
            .ok_or_else(|| ErrorPageStoreError::io("page file has no parent directory".into()))?;
        let write = async {
            tokio::fs::create_dir_all(directory).await?;
            let temporary = directory.join(format!(
                ".{}.tmp-{}",
                path.file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| "userapp-error.html".into()),
                uuid::Uuid::new_v4().simple()
            ));
            tokio::fs::write(&temporary, content).await?;
            // 同目录原子替换（不得先删旧文件再改名）
            tokio::fs::rename(&temporary, path).await?;
            std::io::Result::Ok(())
        };
        if let Err(error) = write.await {
            return Err(ErrorPageStoreError::io(format!(
                "write page file {}: {error}",
                path.display()
            )));
        }
        Ok(())
    }

    async fn delete_file(&self) -> Result<bool, ErrorPageStoreError> {
        match tokio::fs::remove_file(&self.config.authoritative_path).await {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(ErrorPageStoreError::io(format!(
                "remove page file {}: {error}",
                self.config.authoritative_path.display()
            ))),
        }
    }

    async fn save_configmap(&self, content: &[u8]) -> Result<(), ErrorPageStoreError> {
        #[cfg(feature = "kubernetes")]
        {
            crate::userapp_error_page_configmap::upsert_key(
                &self.config,
                content,
                CONFIGMAP_MAX_BYTES,
            )
            .await
            .map_err(map_configmap_error)
        }
        #[cfg(not(feature = "kubernetes"))]
        {
            let _ = content;
            Err(ErrorPageStoreError::io(
                "kubernetes-configmap backend requires a build with the kubernetes feature".into(),
            ))
        }
    }

    async fn delete_configmap_key(&self) -> Result<bool, ErrorPageStoreError> {
        #[cfg(feature = "kubernetes")]
        {
            crate::userapp_error_page_configmap::delete_key(&self.config)
                .await
                .map_err(map_configmap_error)
        }
        #[cfg(not(feature = "kubernetes"))]
        {
            Err(ErrorPageStoreError::io(
                "kubernetes-configmap backend requires a build with the kubernetes feature".into(),
            ))
        }
    }

    /// 权威摘要（file = 文件；configmap = 对象内 key）。
    async fn stored_sha256(&self) -> Result<Option<String>, ErrorPageStoreError> {
        match self.config.backend {
            ErrorPageBackend::File => match read_bounded(&self.config.authoritative_path).await {
                Ok(Some(bytes)) => Ok(Some(sha256_hex(&bytes))),
                Ok(None) => Ok(None),
                Err(error) => Err(ErrorPageStoreError::io(format!(
                    "read stored page {}: {error}",
                    self.config.authoritative_path.display()
                ))),
            },
            ErrorPageBackend::KubernetesConfigmap => {
                #[cfg(feature = "kubernetes")]
                {
                    crate::userapp_error_page_configmap::read_key_sha256(&self.config)
                        .await
                        .map_err(map_configmap_error)
                }
                #[cfg(not(feature = "kubernetes"))]
                {
                    Err(ErrorPageStoreError::io(
                        "kubernetes-configmap backend requires a build with the kubernetes feature"
                            .into(),
                    ))
                }
            }
        }
    }

    /// 按需刷新（节流 + 单执行者；本次响应不等待慢 IO——刷新失败保留旧页）。
    async fn refresh_if_stale(&self) {
        {
            let mut last = self.last_refresh.lock().await;
            if last.is_some_and(|at| at.elapsed() < REFRESH_THROTTLE) {
                return;
            }
            *last = Some(std::time::Instant::now());
        }
        let _guard = self.refresh_gate.lock().await;
        self.load_from_read_path().await;
    }

    /// 一次加载：读投射/文件 → 摘要比较 → 发布缓存或回内置（单一发布路径；
    /// 迟到读取不得覆盖更新后的缓存——摘要相同不重复发布）。
    async fn load_from_read_path(&self) {
        let outcome = match tokio::time::timeout(READ_BUDGET, read_bounded(&self.config.read_path))
            .await
        {
            Ok(Ok(Some(bytes))) if !bytes.is_empty() => match validate_page(&bytes) {
                Ok(()) => LoadOutcome::External { content: bytes },
                Err(error) => {
                    tracing::warn!(
                        "userapp error page file invalid; keeping last valid page: {error}"
                    );
                    LoadOutcome::KeepCurrent("ERROR_PAGE_INVALID")
                }
            },
            // 文件缺失/空：file 后端确定无覆盖 → 内置；configmap 投射缺失可能
            // 只是未挂载/未传播，不据此清缓存。
            Ok(Ok(_)) if self.config.read_missing_is_definitive() => LoadOutcome::NoOverride,
            Ok(Ok(_)) => LoadOutcome::KeepCurrent("ERROR_PAGE_NOT_PROJECTED"),
            Ok(Err(error)) => {
                tracing::warn!("userapp error page read failed; keeping last valid page: {error}");
                LoadOutcome::KeepCurrent("ERROR_PAGE_READ_FAILED")
            }
            Err(_) => {
                tracing::warn!("userapp error page read timed out; keeping last valid page");
                LoadOutcome::KeepCurrent("ERROR_PAGE_READ_FAILED")
            }
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.last_checked_at = Some(now_rfc3339());
        match outcome {
            LoadOutcome::External { content } => {
                let sha = sha256_hex(&content);
                if state.loaded_sha256.as_deref() != Some(sha.as_str()) {
                    self.cache.store(Some(Arc::new(ErrorPageSnapshot {
                        template: Arc::from(String::from_utf8_lossy(&content).into_owned()),
                        sha256: sha.clone(),
                    })));
                    tracing::info!(sha256 = %sha, source = "external", "userapp error page loaded");
                }
                state.loaded_sha256 = Some(sha);
                state.loaded_source = Some("external");
                state.loaded_at = Some(now_rfc3339());
                state.last_load_error_code = None;
            }
            LoadOutcome::NoOverride => {
                self.cache.store(None);
                state.loaded_sha256 = None;
                state.loaded_source = Some("builtin");
                state.loaded_at = Some(now_rfc3339());
                state.last_load_error_code = None;
            }
            LoadOutcome::KeepCurrent(error_code) => {
                state.last_load_error_code = Some(error_code.into());
            }
        }
    }
}

/// 一次加载的三态结果。
enum LoadOutcome {
    External {
        content: Vec<u8>,
    },
    /// 确定无覆盖（内置页生效）
    NoOverride,
    /// 读取/校验失败：保留最近有效页，只记录错误码
    KeepCurrent(&'static str),
}

impl ResolvedConfig {
    /// Ok(None)（文件缺失）是否是"确定无覆盖"——file 后端是；configmap 投射
    /// 缺失可能只是未投射/未挂载，不据此切内置页（保留旧页）。
    fn read_missing_is_definitive(&self) -> bool {
        self.backend == ErrorPageBackend::File
    }
}

impl rcoder_proxy::error_page::ErrorPageSource for UserAppErrorPageService {
    fn current(&self) -> Option<Arc<ErrorPageSnapshot>> {
        self.cache.load_full()
    }
}

#[cfg(feature = "kubernetes")]
fn map_configmap_error(
    error: crate::userapp_error_page_configmap::ConfigmapOpError,
) -> ErrorPageStoreError {
    match error.code {
        "ERROR_PAGE_STORE_CONFLICT" => ErrorPageStoreError::conflict(error.message),
        _ => ErrorPageStoreError::io(error.message),
    }
}

fn backend_name(backend: &ErrorPageBackend) -> &'static str {
    match backend {
        ErrorPageBackend::File => "file",
        ErrorPageBackend::KubernetesConfigmap => "kubernetes-configmap",
    }
}

/// 页面契约校验（上传与加载共用）：非空、UTF-8、≤512KiB、未知
/// `{{RCODER_*}}` 占位符拒绝（四个已知占位符之外不解析其他大括号）。
pub fn validate_page(content: &[u8]) -> std::result::Result<(), String> {
    if content.is_empty() {
        return Err("error page content is empty".into());
    }
    if content.len() > MAX_PAGE_BYTES {
        return Err(format!(
            "error page exceeds {MAX_PAGE_BYTES} bytes (got {})",
            content.len()
        ));
    }
    let text = match std::str::from_utf8(content) {
        Ok(text) => text,
        Err(error) => return Err(format!("error page is not valid UTF-8: {error}")),
    };
    let mut scan = 0;
    while let Some(start) = text[scan..].find("{{RCODER_") {
        let absolute = scan + start;
        let Some(end) = text[absolute..].find("}}") else {
            return Err("unclosed {{RCODER_ placeholder".into());
        };
        let placeholder = &text[absolute..absolute + end + 2];
        let known = matches!(
            placeholder,
            "{{RCODER_TITLE}}"
                | "{{RCODER_MESSAGE}}"
                | "{{RCODER_DIAGNOSTIC_ID}}"
                | "{{RCODER_STATUS}}"
        );
        if !known {
            return Err(format!(
                "unknown placeholder {placeholder} (only RCODER_TITLE/RCODER_MESSAGE/RCODER_DIAGNOSTIC_ID/RCODER_STATUS are supported)"
            ));
        }
        scan = absolute + end + 2;
    }
    Ok(())
}

/// 有界读取（超限截断报错而非整读）。
async fn read_bounded(path: &std::path::Path) -> std::io::Result<Option<Vec<u8>>> {
    match tokio::fs::File::open(path).await {
        Ok(file) => {
            use tokio::io::AsyncReadExt;
            let mut buffer = Vec::new();
            file.take((MAX_PAGE_BYTES as u64) + 1)
                .read_to_end(&mut buffer)
                .await?;
            if buffer.len() > MAX_PAGE_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "page file exceeds size limit",
                ));
            }
            Ok(Some(buffer))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn sha256_hex(content: &[u8]) -> String {
    let digest = Sha256::digest(content);
    hex::encode(digest)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rejects_unknown_placeholder_and_oversize() {
        assert!(validate_page(b"<html>ok</html>").is_ok());
        assert!(validate_page(b"").is_err());
        assert!(validate_page(b"\xff\xfe invalid utf8").is_err());
        assert!(
            validate_page(b"<html>{{RCODER_TITLE}} {{RCODER_MESSAGE}} {{RCODER_DIAGNOSTIC_ID}} {{RCODER_STATUS}}</html>").is_ok()
        );
        assert!(
            validate_page(b"<html>{{RCODER_EVIL}}</html>").is_err(),
            "unknown RCODER placeholder must be rejected at save time"
        );
        assert!(validate_page(b"<html>{{not_rcoder}}</html>").is_ok());
        let oversize = vec![b'a'; MAX_PAGE_BYTES + 1];
        assert!(validate_page(&oversize).is_err());
    }

    #[tokio::test]
    async fn file_backend_roundtrip_save_reload_delete() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("userapp-error.html");
        let config = UserAppErrorPageConfig {
            backend: ErrorPageBackend::File,
            file: Some(path.clone()),
            configmap_name: None,
            configmap_namespace: None,
            configmap_key: None,
            mounted_dir: None,
        };
        let service = Arc::new(UserAppErrorPageService::new(&config).expect("resolve config"));

        // 初始：无覆盖 → 内置页，in_sync=true
        let status = service.admin_status().await.expect("status");
        assert_eq!(status.stored.stored_sha256, None);
        assert!(status.in_sync);

        // 上传 A → 保存 + 本副本立即加载
        let page_a = b"<html>page A {{RCODER_TITLE}}</html>";
        let result = service.save(page_a).await.expect("save A");
        assert!(result.loaded_by_this_replica);
        let status = service.admin_status().await.expect("status");
        assert_eq!(status.stored.stored_sha256, Some(sha256_hex(page_a)));
        assert_eq!(status.loaded_source.as_deref(), Some("external"));
        assert!(status.in_sync);

        // 换 B（重启后语义：新服务实例从权威读回）
        let service_b = Arc::new(UserAppErrorPageService::new(&config).expect("again"));
        let page_b = b"<html>page B</html>";
        service_b.save(page_b).await.expect("save B");
        let status = service_b.admin_status().await.expect("status");
        assert_eq!(status.stored.stored_sha256, Some(sha256_hex(page_b)));
        assert!(status.in_sync);

        // 删除 → 幂等恢复内置
        service_b.delete().await.expect("delete");
        let status = service_b.admin_status().await.expect("status");
        assert_eq!(status.stored.stored_sha256, None);
        assert!(status.in_sync, "builtin on both sides must be in_sync");
        service_b
            .delete()
            .await
            .expect("delete again is idempotent");

        // 坏内容保存被拒绝且保留旧页
        service_b
            .save(b"<html>{{RCODER_BAD}}</html>")
            .await
            .expect_err("invalid placeholder rejected");
        let status = service_b.admin_status().await.expect("status");
        assert_eq!(
            status.stored.stored_sha256, None,
            "rejected save must not alter authority"
        );
    }

    #[tokio::test]
    async fn file_backend_keeps_old_page_on_read_failure() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("userapp-error.html");
        let config = UserAppErrorPageConfig {
            backend: ErrorPageBackend::File,
            file: Some(path.clone()),
            configmap_name: None,
            configmap_namespace: None,
            configmap_key: None,
            mounted_dir: None,
        };
        let service = Arc::new(UserAppErrorPageService::new(&config).expect("resolve"));
        let page = b"<html>good page</html>";
        service.save(page).await.expect("save");
        // 权威文件被外部替换为坏内容（手工维护场景）：缓存保留最近有效页。
        tokio::fs::write(&path, b"\xff\xfe broken")
            .await
            .expect("break file");
        // 绕过节流直接触发一次加载
        service.load_from_read_path().await;
        let status = service.admin_status().await.expect("status");
        assert_eq!(
            status.loaded_sha256,
            Some(sha256_hex(page)),
            "invalid file must not evict the last valid page"
        );
    }
}

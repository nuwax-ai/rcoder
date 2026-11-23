//! E2B 兼容沙箱编排服务（M1：单副本）。
//!
//! 职责：模板注册种子、API key 鉴权、create 全流程（Pod → envd /init → Ready）、
//! kill/timeout/get/list、数据面 token 授权、超时回收。
//!
//! 关键不变量：
//! - 沙箱 Pod 一律 Guaranteed（构造见 docker_manager k8s_sandbox）。
//! - 变更顺序 = 先持久化意图再产生外部副作用；外部副作用结果未知时落
//!   CleanupPending 交回收器对账，绝不静默宣称完成。
//! - envd `/init` 重试 = 同 token + 新 timestamp（envd 源码实证幂等）。
use std::sync::Arc;

use rand::RngExt;
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};

use shared_types::sandbox::{
    CreateSandboxParams, SandboxApiKeyRecord, SandboxInstanceRecord, SandboxInstanceState,
    SandboxStore, SandboxStoreError, SandboxTemplateSpec, sha256_hex,
};

use crate::config::E2bSandboxConfig;
use docker_manager::runtime::k8s_sandbox::{SandboxK8s, SandboxK8sError, SandboxPodParams};

pub const ENVD_PORT: i32 = 49983;

/// kill 的处置资格来源：决定 CAS 重试重读后是否复核原判定仍成立。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillReason {
    /// 用户显式 kill / 认领失败回收 / 创建失败回收：无需复核（操作者持有最新事实）。
    Explicit,
    /// 回收器按到期扫描：重读后必须复核 expires 仍 <= now——并发续期/温池认领
    /// 赢得 CAS 时放弃处置，绝不误杀刚续期或刚被认领的实例。
    ReapExpired,
    /// 重启恢复按陈旧状态判定：重读后复核非终态即可（竞态窗口为启动一次性）。
    Recover,
}

/// 温池占位实例的 key_id（seed_registry 注入系统 key 满足外键；secret 是
/// 永不签发的常量且 seed 时即置为已过期，无法通过 authenticate）。
pub const WARM_POOL_KEY_ID: &str = "_warm-pool";

#[derive(Debug, thiserror::Error)]
pub enum SandboxApiError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("conflict: {0}")]
    Conflict(String),
    /// 参数/状态错误带错误码：HTTP 边缘按 Accept-Language 翻译
    /// （en 默认）；动态细节（模板名/数值）由构造点记日志，不进响应。
    #[error("invalid: code {0}")]
    InvalidCode(&'static str),
    #[error("conflict: code {0}")]
    ConflictCode(&'static str),
    #[error("capability not supported: {0}")]
    NotSupported(&'static str),
    #[error("unavailable: {0}")]
    Unavailable(String),
}

fn store_error(error: SandboxStoreError) -> SandboxApiError {
    match error {
        SandboxStoreError::Conflict(context) => SandboxApiError::Conflict(context),
        SandboxStoreError::InvalidOperation(context) => SandboxApiError::Invalid(context),
        other => SandboxApiError::Unavailable(other.to_string()),
    }
}

fn k8s_error(error: SandboxK8sError) -> SandboxApiError {
    SandboxApiError::Unavailable(error.to_string())
}

pub struct CreateOutcome {
    pub record: SandboxInstanceRecord,
    /// 明文 envdAccessToken（仅 create 响应这一次下发完整值）。
    pub token: String,
}

pub struct SandboxManager {
    store: Arc<dyn SandboxStore>,
    k8s: SandboxK8s,
    http: reqwest::Client,
    default_ttl_seconds: i64,
    max_ttl_seconds: i64,
    create_ready_timeout: Duration,
    envd_init_retries: u32,
    cipher: Aes256Gcm,
    public_domain: String,
    volume_storage_class: String,
    volume_max_size_gib: i32,
    warm_pool_enabled: bool,
    warm_idle_ttl_seconds: i64,
}

fn random_id(len: usize) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdef";
    let mut rng = rand::rng();
    (0..len)
        .map(|_| ALPHABET[rng.random_range(0u8..16) as usize] as char)
        .collect()
}

fn random_token() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..32)
        .map(|_| ALPHABET[rng.random_range(0u8..36) as usize] as char)
        .collect()
}

impl SandboxManager {
    /// 组装；`token_key_hex` 必须是 64 位 hex（32 字节 AES-256 key）。
    pub fn new(
        store: Arc<dyn SandboxStore>,
        k8s: SandboxK8s,
        config: &E2bSandboxConfig,
    ) -> Result<Self, String> {
        let key_bytes = hex::decode(config.token_key_hex.trim())
            .map_err(|error| format!("e2b_sandbox token_key_hex 解析失败: {error}"))?;
        // 长度校验必须在 from_slice 之前——它对错误长度是 panic 而非错误。
        if key_bytes.len() != 32 {
            return Err(format!(
                "e2b_sandbox token_key_hex 长度错误: {} 字节（需要 32）",
                key_bytes.len()
            ));
        }
        let key = aes_gcm::Key::<Aes256Gcm>::from_slice(&key_bytes);
        Ok(Self {
            store,
            k8s,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                // 禁止跟随重定向：沙箱侧 3xx 不得让 rcoder 进程向授权外的
                // 目标发起请求（SSRF 面）；上游 3xx 原样交由调用方处理。
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|error| format!("e2b_sandbox http client: {error}"))?,
            default_ttl_seconds: config.default_ttl_seconds,
            max_ttl_seconds: config.max_ttl_seconds,
            create_ready_timeout: Duration::from_secs(config.create_ready_timeout_seconds),
            envd_init_retries: config.envd_init_retries.max(1),
            cipher: Aes256Gcm::new(key),
            public_domain: config.public_domain.clone(),
            volume_storage_class: config.volume_storage_class.clone(),
            volume_max_size_gib: config.volume_max_size_gib,
            warm_pool_enabled: config.warm_pool_enabled,
            warm_idle_ttl_seconds: config.warm_idle_ttl_seconds.max(60),
        })
    }

    /// 配置驱动的幂等种子（api key 哈希 + 模板注册表）。
    pub async fn seed_registry(&self, config: &E2bSandboxConfig) -> Result<(), SandboxApiError> {
        // 温池系统 key：sandbox_instances.key_id 有外键，占位实例必须引用一个真实行；
        // expires=1（1970）使其永远无法通过 authenticate。
        self.store
            .seed_api_key(&SandboxApiKeyRecord {
                key_id: WARM_POOL_KEY_ID.to_string(),
                tenant_id: "_warm_pool".to_string(),
                public_prefix: "_warm-po".to_string(),
                key_hash: sha256_hex("rcoder:e2b:warm-pool:never-issue"),
                scopes: "sandbox".to_string(),
                expires_at_us: Some(1),
                revoked_at_us: None,
            })
            .await
            .map_err(store_error)?;
        for seed in &config.api_keys {
            self.store
                .seed_api_key(&SandboxApiKeyRecord {
                    key_id: seed.key_id.clone(),
                    tenant_id: seed.tenant_id.clone(),
                    public_prefix: seed.secret.chars().take(8).collect(),
                    key_hash: sha256_hex(&seed.secret),
                    scopes: "sandbox".to_string(),
                    expires_at_us: None,
                    revoked_at_us: None,
                })
                .await
                .map_err(store_error)?;
        }
        for template in &config.templates {
            let spec = SandboxTemplateSpec {
                template_id: template.template_id.clone(),
                revision: template.revision,
                image: template.image.clone(),
                image_digest: template.image_digest.clone(),
                envd_version: template.envd_version.clone(),
                runtime_class: template
                    .runtime_class
                    .clone()
                    .filter(|value| !value.is_empty()),
                cpu_millicores: template.cpu_millicores,
                memory_mib: template.memory_mib,
                default_user: template.default_user.clone(),
                default_workdir: template.default_workdir.clone(),
                default_env: template.default_env.clone().into_iter().collect(),
                ttl_max_seconds: template.ttl_max_seconds,
                warm_target: template.warm_target,
            };
            self.store
                .seed_template(
                    &shared_types::sandbox::SandboxTemplateSummary {
                        template_id: template.template_id.clone(),
                        display_name: template.display_name.clone(),
                        enabled: template.enabled,
                        current_revision: template.revision,
                    },
                    &spec,
                )
                .await
                .map_err(store_error)?;
        }
        Ok(())
    }

    pub fn public_domain(&self) -> &str {
        &self.public_domain
    }

    pub async fn list_templates(
        &self,
    ) -> Result<Vec<shared_types::sandbox::SandboxTemplateSummary>, SandboxApiError> {
        self.store.list_templates().await.map_err(store_error)
    }

    pub async fn authenticate(
        &self,
        provided_key: &str,
    ) -> Result<SandboxApiKeyRecord, SandboxApiError> {
        let record = self
            .store
            .find_api_key_by_hash(&sha256_hex(provided_key))
            .await
            .map_err(store_error)?
            .ok_or(SandboxApiError::Unauthorized)?;
        if record.revoked_at_us.is_some() {
            return Err(SandboxApiError::Unauthorized);
        }
        if record
            .expires_at_us
            .is_some_and(|expires| expires <= chrono::Utc::now().timestamp_micros())
        {
            return Err(SandboxApiError::Unauthorized);
        }
        Ok(record)
    }

    fn encrypt_token(&self, token: &str) -> Result<String, SandboxApiError> {
        let nonce_bytes = rand::rng().random::<[u8; 12]>();
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(nonce, token.as_bytes())
            .map_err(|error| SandboxApiError::Unavailable(format!("token encrypt: {error}")))?;
        let mut packed = nonce_bytes.to_vec();
        packed.extend_from_slice(&ciphertext);
        Ok(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &packed,
        ))
    }

    pub fn decrypt_token(&self, record: &SandboxInstanceRecord) -> Result<String, SandboxApiError> {
        let packed = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            record.encrypted_token.as_deref().unwrap_or_default(),
        )
        .map_err(|error| SandboxApiError::Unavailable(format!("token b64: {error}")))?;
        if packed.len() < 12 {
            return Err(SandboxApiError::Unavailable(
                "token ciphertext is corrupted".into(),
            ));
        }
        let (nonce_bytes, ciphertext) = packed.split_at(12);
        self.cipher
            .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .map_err(|_| SandboxApiError::Unauthorized)
    }

    /// create 全流程：意图 → Pod → Ready → envd /init → Ready 发布。
    pub async fn create(
        &self,
        key: &SandboxApiKeyRecord,
        params: &CreateSandboxParams,
    ) -> Result<CreateOutcome, SandboxApiError> {
        // 幂等：同 Idempotency-Key 的非终态实例直接返回（网络重试去重）。
        // 仅租户内匹配 + 仅 Ready 视为可复用成功；进行中/清理中返回 409 让
        // 客户端重试（官方幂等语义：创建未完成不得当成功发布）。
        if let Some(idem_key) = &params.idempotency_key
            && let Some(existing) = self
                .store
                .find_by_idempotency_key(&key.tenant_id, idem_key)
                .await
                .map_err(store_error)?
        {
            return match existing.desired_state {
                SandboxInstanceState::Ready => {
                    tracing::info!(sandbox_id = %existing.sandbox_id, "e2b create 幂等命中，返回已有实例");
                    let token = self.decrypt_token(&existing)?;
                    Ok(CreateOutcome {
                        record: existing,
                        token,
                    })
                }
                state => {
                    tracing::info!(?state, "e2b create idempotency conflict");
                    Err(SandboxApiError::ConflictCode(
                        shared_types::error_codes::ERR_E2B_IDEMPOTENCY_IN_PROGRESS,
                    ))
                }
            };
        }

        // 温池认领：找 WarmReady 实例 → 原子认领 → /init → Ready（跳过 Pod 创建）。
        // 卷挂载只能冷启动——温池 Pod 创建时没有任何 PVC。
        if self.warm_pool_enabled
            && params.volume_mounts.is_empty()
            && let Some(outcome) = self.try_claim_warm(key, params).await?
        {
            return Ok(outcome);
        }

        let spec = self
            .store
            .get_template_spec(&params.template_id)
            .await
            .map_err(store_error)?
            .ok_or_else(|| {
                tracing::info!(template = %params.template_id, "e2b template unavailable");
                SandboxApiError::InvalidCode(
                    shared_types::error_codes::ERR_E2B_TEMPLATE_UNAVAILABLE,
                )
            })?;
        let ttl_seconds = params.timeout_seconds.unwrap_or(self.default_ttl_seconds);
        if ttl_seconds <= 0 {
            return Err(SandboxApiError::InvalidCode(
                shared_types::error_codes::ERR_E2B_TIMEOUT_INVALID,
            ));
        }
        let ttl_cap = self.max_ttl_seconds.min(spec.ttl_max_seconds);
        if ttl_seconds > ttl_cap {
            tracing::info!(ttl_seconds, ttl_cap, "e2b timeout exceeds cap");
            return Err(SandboxApiError::InvalidCode(
                shared_types::error_codes::ERR_E2B_TIMEOUT_EXCEEDS_CAP,
            ));
        }

        let sandbox_id = random_id(12);
        let pod_name = format!("e2b-{sandbox_id}");
        let token = random_token();
        let token_hash = sha256_hex(&token);
        let encrypted_token = self.encrypt_token(&token)?;

        let now = chrono::Utc::now().timestamp_micros();
        // hard_deadline：不可续期硬顶 = 创建时刻 + 模板最大存活 + 创建等待余量。
        let hard_deadline_us = now
            + (spec.ttl_max_seconds + self.create_ready_timeout.as_secs() as i64 + 120) * 1_000_000;

        let record = SandboxInstanceRecord {
            sandbox_id: sandbox_id.clone(),
            tenant_id: key.tenant_id.clone(),
            key_id: key.key_id.clone(),
            template_id: spec.template_id.clone(),
            template_revision: spec.revision,
            image: spec.image.clone(),
            image_digest: spec.image_digest.clone(),
            envd_version: spec.envd_version.clone(),
            runtime_class: spec.runtime_class.clone(),
            cpu_millicores: spec.cpu_millicores,
            memory_mib: spec.memory_mib,
            desired_state: SandboxInstanceState::Provisioning,
            observed_state: None,
            instance_revision: 1,
            namespace: self.k8s.namespace().to_string(),
            pod_name,
            pod_uid: None,
            token_hash,
            encrypted_token: Some(encrypted_token),
            token_generation: 1,
            env: params.env.clone(),
            metadata: params.metadata.clone(),
            idempotency_key: params.idempotency_key.clone(),
            failure_reason: None,
            created_at_us: now,
            updated_at_us: now,
            ready_at_us: None,
            expires_at_us: None,
            hard_deadline_us: Some(hard_deadline_us),
            deleted_at_us: None,
        };
        // Volume 挂载校验前置（纯读操作，insert 之前——卷名错误不泄漏 Provisioning 行）
        let mut resolved_volume_mounts = Vec::new();
        for mount in &params.volume_mounts {
            if !mount.mount_path.starts_with('/') {
                return Err(SandboxApiError::Invalid(format!(
                    "mountPath \"{}\" must start with /",
                    mount.mount_path
                )));
            }
            if mount.mount_path.contains("..") {
                return Err(SandboxApiError::Invalid(
                    "mountPath must not contain path traversal".into(),
                ));
            }
            const SENSITIVE: &[&str] = &["/proc", "/sys", "/dev", "/etc", "/var/run", "/run"];
            if SENSITIVE
                .iter()
                .any(|p| mount.mount_path == *p || mount.mount_path.starts_with(&format!("{}/", p)))
            {
                return Err(SandboxApiError::Invalid(format!(
                    "mountPath \"{}\" must not mount a system path",
                    mount.mount_path
                )));
            }
            let volume = self
                .store
                .find_volume_by_name(&key.tenant_id, &mount.volume_name)
                .await
                .map_err(store_error)?
                .ok_or_else(|| {
                    SandboxApiError::Invalid(format!(
                        "volume \"{}\" does not exist",
                        mount.volume_name
                    ))
                })?;
            resolved_volume_mounts.push((volume.pvc_name.clone(), mount.mount_path.clone()));
        }

        // 并发同 Idempotency-Key 插入命中条件唯一索引（Conflict）→ 读回已有实例：
        // 仅租户内且 Ready 才按成功返回（跨租户同键/进行中一律 409，绝不解密
        // 他人实例）；与前置 find_by_idempotency_key 构成双保险。
        let record = match self.store.insert_instance(&record).await {
            Ok(inserted) => inserted,
            Err(SandboxStoreError::Conflict(reason)) => {
                if let Some(idem_key) = &params.idempotency_key
                    && let Some(existing) = self
                        .store
                        .find_by_idempotency_key(&key.tenant_id, idem_key)
                        .await
                        .map_err(store_error)?
                {
                    if existing.desired_state == SandboxInstanceState::Ready {
                        tracing::info!(
                            sandbox_id = %existing.sandbox_id,
                            "e2b create 幂等冲突收敛，返回已有实例"
                        );
                        let token = self.decrypt_token(&existing)?;
                        return Ok(CreateOutcome {
                            record: existing,
                            token,
                        });
                    }
                    return Err(SandboxApiError::Conflict(format!(
                        "a sandbox with the same idempotency key is in state {:?}; retry later",
                        existing.desired_state
                    )));
                }
                return Err(SandboxApiError::Conflict(reason));
            }
            Err(error) => return Err(store_error(error)),
        };

        // 外部副作用：Pod 创建 + 等待 Ready。
        let pod_params = SandboxPodParams {
            name: record.pod_name.clone(),
            image: record.image.clone(),
            runtime_class: record.runtime_class.clone(),
            cpu_millicores: record.cpu_millicores,
            memory_mib: record.memory_mib,
            envd_port: ENVD_PORT,
            max_lifetime_seconds: spec.ttl_max_seconds
                + self.create_ready_timeout.as_secs() as i64
                + 120,
            env: Vec::new(),
            volume_mounts: resolved_volume_mounts,
        };
        if let Err(error) = self.k8s.create_pod(&pod_params).await {
            return self
                .fail_create(&record, &format!("create pod: {error}"))
                .await;
        }
        let status = match self
            .k8s
            .wait_ready(&record.pod_name, self.create_ready_timeout)
            .await
        {
            Ok(status) => status,
            Err(error) => {
                return self
                    .fail_create(&record, &format!("wait ready: {error}"))
                    .await;
            }
        };
        let revision = record.instance_revision;
        let record = self
            .store
            .transition_state(
                &record.sandbox_id,
                revision,
                SandboxInstanceState::Initializing,
                None,
                Some(&status.phase),
            )
            .await
            .map_err(store_error)?;

        // envd /init（同 token + 新 timestamp 重试；4xx 不重试直接失败）。
        let mut env_map = spec.default_env.clone();
        for (key, value) in &params.env {
            env_map.push((key.clone(), value.clone()));
        }
        let pod_ip = status.pod_ip.clone().unwrap_or_default();
        match self.envd_init(&pod_ip, &token, &spec, &env_map).await {
            Ok(()) => {}
            Err(error) => {
                return self
                    .fail_create(&record, &format!("envd init: {error}"))
                    .await;
            }
        }

        let ready_at = chrono::Utc::now().timestamp_micros();
        let record = self
            .store
            .mark_ready(
                &record.sandbox_id,
                record.instance_revision,
                status.pod_uid.as_deref(),
                ready_at,
                ready_at + ttl_seconds * 1_000_000,
            )
            .await
            .map_err(store_error)?;
        tracing::info!(
            sandbox_id = %record.sandbox_id,
            template = %record.template_id,
            "e2b sandbox ready"
        );
        Ok(CreateOutcome { record, token })
    }

    async fn fail_create(
        &self,
        record: &SandboxInstanceRecord,
        reason: &str,
    ) -> Result<CreateOutcome, SandboxApiError> {
        tracing::warn!(sandbox_id = %record.sandbox_id, reason, "e2b sandbox create failed");
        // Pod 删除结果未知/失败 → CleanupPending 交回收器；成功 → Failed 终态。
        match self.k8s.delete_pod(&record.pod_name).await {
            Ok(_) => {
                let _unused = self
                    .store
                    .finish(
                        &record.sandbox_id,
                        record.instance_revision,
                        SandboxInstanceState::Failed,
                        Some(reason),
                    )
                    .await;
            }
            Err(delete_error) => {
                let _unused = self
                    .store
                    .transition_state(
                        &record.sandbox_id,
                        record.instance_revision,
                        SandboxInstanceState::CleanupPending,
                        Some(&format!("{reason}; pod delete: {delete_error}")),
                        None,
                    )
                    .await;
            }
        }
        Err(SandboxApiError::Unavailable(format!(
            "create failed: {reason}"
        )))
    }

    async fn envd_init(
        &self,
        pod_ip: &str,
        token: &str,
        spec: &SandboxTemplateSpec,
        env_map: &[(String, String)],
    ) -> Result<(), String> {
        let url = format!("http://{pod_ip}:{ENVD_PORT}/init");
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let envs: std::collections::BTreeMap<String, String> =
                env_map.iter().cloned().collect();
            let body = serde_json::json!({
                "accessToken": token,
                "defaultUser": spec.default_user,
                "defaultWorkdir": spec.default_workdir,
                "envVars": envs,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            });
            match self.http.post(&url).json(&body).send().await {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) if response.status().is_client_error() => {
                    return Err(format!("envd /init 4xx: {}", response.status()));
                }
                Ok(response) => {
                    let status = response.status();
                    if attempt >= self.envd_init_retries {
                        return Err(format!(
                            "envd /init {status} 重试 {} 次后放弃",
                            self.envd_init_retries
                        ));
                    }
                }
                Err(error) => {
                    if attempt >= self.envd_init_retries {
                        return Err(format!("envd /init 网络错误: {error}"));
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(300 * attempt as u64)).await;
        }
    }

    pub async fn get(
        &self,
        key: &SandboxApiKeyRecord,
        sandbox_id: &str,
    ) -> Result<SandboxInstanceRecord, SandboxApiError> {
        let record = self
            .store
            .get_instance(sandbox_id)
            .await
            .map_err(store_error)?
            .ok_or_else(|| SandboxApiError::NotFound(sandbox_id.to_string()))?;
        ensure_tenant(&record, key)?;
        if record.desired_state.terminal() {
            // 终态对活跃查询表现为不存在（诊断走管理面）。
            return Err(SandboxApiError::NotFound(sandbox_id.to_string()));
        }
        Ok(record)
    }

    pub async fn list(
        &self,
        key: &SandboxApiKeyRecord,
        before: Option<(i64, &str)>,
        limit: usize,
    ) -> Result<Vec<SandboxInstanceRecord>, SandboxApiError> {
        let records = self
            .store
            .list_instances(&key.tenant_id, before, limit)
            .await
            .map_err(store_error)?;
        Ok(records
            .into_iter()
            .filter(|record| !record.desired_state.terminal())
            .collect())
    }

    /// 幂等 kill：终态重复调用返回成功。CAS 重试防竞争（续期/回收/并发 kill）。
    /// 撤销 token（状态置 Terminating + expires=0）→ 删 Pod → Deleted。
    pub async fn kill(
        &self,
        key: &SandboxApiKeyRecord,
        sandbox_id: &str,
    ) -> Result<(), SandboxApiError> {
        let record = self
            .store
            .get_instance(sandbox_id)
            .await
            .map_err(store_error)?
            .ok_or_else(|| SandboxApiError::NotFound(sandbox_id.to_string()))?;
        ensure_tenant(&record, key)?;
        if record.desired_state.terminal() {
            return Ok(());
        }
        self.kill_by_record(&record, KillReason::Explicit).await
    }

    /// 内部 kill（回收器/重启恢复直接调，不做租户检查）。CAS 重试上限 5 次，
    /// 每次重读后按 KillReason 复核处置资格。数据面授权立即失效的实际机制是
    /// 状态检查 + expires=0（authorize_data_plane 拒绝非 Ready 或已到期实例）。
    pub async fn kill_by_record(
        &self,
        initial: &SandboxInstanceRecord,
        reason: KillReason,
    ) -> Result<(), SandboxApiError> {
        let mut record = initial.clone();
        for attempt in 0..5 {
            if record.desired_state.terminal() {
                return Ok(());
            }
            if let KillReason::ReapExpired = reason {
                // 处置资格复核：并发续期（expires 推后）或温池认领（expires 置
                // NULL、状态 Initializing）都使本方丧失回收资格。
                let now = chrono::Utc::now().timestamp_micros();
                if !record.expires_at_us.is_some_and(|expires| expires <= now) {
                    tracing::info!(
                        sandbox_id = %record.sandbox_id,
                        ?record.expires_at_us,
                        "e2b reaper: 到期资格已被并发操作刷新，放弃回收"
                    );
                    return Ok(());
                }
            }
            // 第一步：Terminating + expires=0（CAS；冲突则重读重试）。
            match self
                .store
                .transition_state(
                    &record.sandbox_id,
                    record.instance_revision,
                    SandboxInstanceState::Terminating,
                    None,
                    None,
                )
                .await
            {
                Ok(mut updated) => {
                    // expires=0 让数据面立即失效。两步必须都落库：set_expires
                    // 失败（并发 set_timeout 赢了 CAS）时重读重试而非吞错——
                    // 否则 Terminating 行残留旧 expires（甚至 NULL），回收
                    // 扫描按 expires 过滤会永久漏掉它（复审 P2）。
                    match self
                        .store
                        .set_expires(&record.sandbox_id, updated.instance_revision, 0)
                        .await
                    {
                        Ok(_) => {
                            updated = self
                                .store
                                .get_instance(&record.sandbox_id)
                                .await
                                .map_err(store_error)?
                                .unwrap_or(updated);
                            record = updated;
                            break;
                        }
                        Err(SandboxStoreError::Conflict(_)) if attempt < 4 => {
                            tracing::debug!(
                                sandbox_id = %record.sandbox_id,
                                attempt,
                                "kill set_expires CAS 冲突，重读重试"
                            );
                            record = self
                                .store
                                .get_instance(&record.sandbox_id)
                                .await
                                .map_err(store_error)?
                                .ok_or_else(|| {
                                    SandboxApiError::NotFound(record.sandbox_id.clone())
                                })?;
                            continue;
                        }
                        Err(error) => return Err(store_error(error)),
                    }
                }
                Err(SandboxStoreError::Conflict(_)) if attempt < 4 => {
                    tracing::debug!(sandbox_id = %record.sandbox_id, attempt, "kill CAS 冲突，重读重试");
                    record = self
                        .store
                        .get_instance(&record.sandbox_id)
                        .await
                        .map_err(store_error)?
                        .ok_or_else(|| SandboxApiError::NotFound(record.sandbox_id.clone()))?;
                    continue;
                }
                Err(error) => return Err(store_error(error)),
            }
        }
        // 第二步：删 Pod（UID 不匹配也视为已删除——外部并发删除是正常场景）。
        match self.k8s.delete_pod(&record.pod_name).await {
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(sandbox_id = %record.sandbox_id, error = %error.to_string(), "kill: pod 删除失败，置 CleanupPending");
                let _unused = self
                    .store
                    .transition_state(
                        &record.sandbox_id,
                        record.instance_revision,
                        SandboxInstanceState::CleanupPending,
                        Some(&format!("pod delete: {error}")),
                        None,
                    )
                    .await;
                return Ok(());
            }
        }
        // 第三步：终态 Deleted。
        let _unused = self
            .store
            .finish(
                &record.sandbox_id,
                record.instance_revision,
                SandboxInstanceState::Deleted,
                None,
            )
            .await;
        tracing::info!(sandbox_id = %record.sandbox_id, "e2b sandbox killed");
        Ok(())
    }

    /// 重启恢复：扫描非终态记录，与实际 Pod 对账收敛。
    /// - Pod 不存在 → Deleted（终态化）
    /// - Pod 存在但 Provisioning/Initializing 超时 → kill
    /// - Pod 存在且 Ready → 检查 envd 存活，不活则 kill
    ///
    /// 在 seed_registry 之后、run_reaper 启动之前调用。
    pub async fn recover_orphans(&self) {
        {
            let records = match self.store.list_all_non_terminal().await {
                Ok(list) => list,
                Err(error) => {
                    tracing::warn!(error = %error, "recover: 扫描失败");
                    return;
                }
            };
            for record in records {
                let pod_status = self.k8s.pod_status(&record.pod_name).await;
                match pod_status {
                    // Pod 不存在 → 终态化
                    Ok(None) => {
                        tracing::info!(sandbox_id = %record.sandbox_id, ?record.desired_state, "recover: pod 不存在，终态化");
                        let _unused = self
                            .store
                            .finish(
                                &record.sandbox_id,
                                record.instance_revision,
                                SandboxInstanceState::Deleted,
                                Some("recovered: pod absent"),
                            )
                            .await;
                    }
                    // Pod 存在
                    Ok(Some(status)) => {
                        match record.desired_state {
                            // 非终态且 Pod 还在 → 尝试清理
                            SandboxInstanceState::Terminating
                            | SandboxInstanceState::CleanupPending => {
                                tracing::info!(sandbox_id = %record.sandbox_id, "recover: 清理残留 Pod");
                                // 删除结果未知/失败不得写终态——保留可重试状态
                                // （CleanupPending 由 reaper 兜底再扫）。
                                match self.k8s.delete_pod(&record.pod_name).await {
                                    Ok(_) => {
                                        let _unused = self
                                            .store
                                            .finish(
                                                &record.sandbox_id,
                                                record.instance_revision,
                                                SandboxInstanceState::Deleted,
                                                Some("recovered: cleanup"),
                                            )
                                            .await;
                                    }
                                    Err(delete_error) => {
                                        tracing::warn!(
                                            sandbox_id = %record.sandbox_id,
                                            error = %delete_error.to_string(),
                                            "recover: Pod 删除失败，保留待清理状态"
                                        );
                                        let _unused = self
                                            .store
                                            .transition_state(
                                                &record.sandbox_id,
                                                record.instance_revision,
                                                SandboxInstanceState::CleanupPending,
                                                Some(&format!("pod delete: {delete_error}")),
                                                None,
                                            )
                                            .await;
                                    }
                                }
                            }
                            // Provisioning/Initializing 且 Pod 终相 → kill
                            SandboxInstanceState::Provisioning
                            | SandboxInstanceState::Initializing
                                if matches!(status.phase.as_str(), "Failed" | "Succeeded") =>
                            {
                                tracing::info!(sandbox_id = %record.sandbox_id, phase = %status.phase, "recover: Pod 终相，kill");
                                let _unused =
                                    self.kill_by_record(&record, KillReason::Recover).await;
                            }
                            // Ready 且 Pod 非 Running → kill
                            SandboxInstanceState::Ready if status.phase != "Running" => {
                                tracing::info!(sandbox_id = %record.sandbox_id, phase = %status.phase, "recover: Ready 但 Pod 非 Running，kill");
                                let _unused =
                                    self.kill_by_record(&record, KillReason::Recover).await;
                            }
                            // Ready + Running → envd 存活检查（Pod 活着但 envd 死掉的
                            // 场景正是生产事故里"沙箱不响应"的形态）
                            SandboxInstanceState::Ready => {
                                let Some(pod_ip) = status.pod_ip.clone() else {
                                    continue;
                                };
                                if !self.envd_alive(&pod_ip).await {
                                    tracing::warn!(sandbox_id = %record.sandbox_id, "recover: envd 无响应，kill");
                                    let _unused =
                                        self.kill_by_record(&record, KillReason::Recover).await;
                                }
                            }
                            // WarmReady 且 Pod 非 Running（节点回收等）→ kill 交回收器补池
                            SandboxInstanceState::WarmReady if status.phase != "Running" => {
                                tracing::info!(sandbox_id = %record.sandbox_id, phase = %status.phase, "recover: 温池 Pod 非 Running，kill");
                                let _unused =
                                    self.kill_by_record(&record, KillReason::Recover).await;
                            }
                            // Provisioning/Initializing 超时（创建时间 + 超时上限 < now）→ kill
                            SandboxInstanceState::Provisioning
                            | SandboxInstanceState::Initializing => {
                                let deadline = record.created_at_us
                                    + (self.create_ready_timeout.as_secs() as i64 + 120)
                                        * 1_000_000;
                                if chrono::Utc::now().timestamp_micros() > deadline {
                                    tracing::info!(sandbox_id = %record.sandbox_id, "recover: 创建超时，kill");
                                    let _unused =
                                        self.kill_by_record(&record, KillReason::Recover).await;
                                }
                            }
                            _ => {} // Ready + Running = 健康，跳过
                        }
                    }
                    Err(error) => {
                        tracing::warn!(sandbox_id = %record.sandbox_id, error = %error.to_string(), "recover: Pod 查询失败");
                    }
                }
            }
        }
    }

    /// envd 存活探测：任何 HTTP 应答（含 404）都算进程活着；连续 3 次网络层
    /// 失败才判死，避免 CNI 抖动误杀。
    async fn envd_alive(&self, pod_ip: &str) -> bool {
        let url = format!("http://{pod_ip}:{ENVD_PORT}/health");
        for attempt in 1..=3u32 {
            match self
                .http
                .get(&url)
                .timeout(Duration::from_secs(2))
                .send()
                .await
            {
                Ok(_) => return true,
                Err(_) if attempt == 3 => return false,
                Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
        false
    }

    pub async fn set_timeout(
        &self,
        key: &SandboxApiKeyRecord,
        sandbox_id: &str,
        timeout_seconds: i64,
    ) -> Result<(), SandboxApiError> {
        let record = self.get(key, sandbox_id).await?;
        if record.desired_state != SandboxInstanceState::Ready {
            return Err(SandboxApiError::InvalidCode(
                shared_types::error_codes::ERR_E2B_RENEW_REQUIRES_READY,
            ));
        }
        if timeout_seconds <= 0 {
            return Err(SandboxApiError::InvalidCode(
                shared_types::error_codes::ERR_E2B_TIMEOUT_INVALID,
            ));
        }
        let spec_cap = self.max_ttl_seconds.min(
            self.store
                .get_template_spec(&record.template_id)
                .await
                .map_err(store_error)?
                .map(|spec| spec.ttl_max_seconds)
                .unwrap_or(self.max_ttl_seconds),
        );
        if timeout_seconds > spec_cap {
            return Err(SandboxApiError::Invalid(format!(
                "timeout {timeout_seconds}s 超过上限 {spec_cap}s"
            )));
        }
        // CAS 重试：续期与回收/kill 可能并发
        let mut record = record;
        for attempt in 0..5 {
            let now = chrono::Utc::now().timestamp_micros();
            let new_expires = now + timeout_seconds * 1_000_000;
            if let Some(hard) = record.hard_deadline_us
                && new_expires > hard
            {
                return Err(SandboxApiError::Invalid(
                    "续期越过硬顶（hard_deadline）".into(),
                ));
            }
            match self
                .store
                .set_expires(sandbox_id, record.instance_revision, new_expires)
                .await
            {
                Ok(_) => return Ok(()),
                Err(SandboxStoreError::Conflict(_)) if attempt < 4 => {
                    tracing::debug!(sandbox_id, attempt, "set_timeout CAS 冲突，重读重试");
                    record = self
                        .store
                        .get_instance(sandbox_id)
                        .await
                        .map_err(store_error)?
                        .ok_or_else(|| SandboxApiError::NotFound(sandbox_id.to_string()))?;
                    if record.desired_state != SandboxInstanceState::Ready {
                        return Err(SandboxApiError::InvalidCode(
                            shared_types::error_codes::ERR_E2B_NOT_READY,
                        ));
                    }
                    continue;
                }
                Err(error) => return Err(store_error(error)),
            }
        }
        Err(SandboxApiError::Conflict(
            "set_timeout CAS 重试 5 次后放弃".into(),
        ))
    }

    /// 数据面授权：沙箱存在、Ready、未到期、token 匹配。
    pub async fn authorize_data_plane(
        &self,
        sandbox_id: &str,
        token: &str,
    ) -> Result<SandboxInstanceRecord, SandboxApiError> {
        let record = self
            .store
            .get_instance(sandbox_id)
            .await
            .map_err(store_error)?
            .ok_or_else(|| SandboxApiError::NotFound(sandbox_id.to_string()))?;
        if record.desired_state != SandboxInstanceState::Ready {
            return Err(SandboxApiError::NotFound(sandbox_id.to_string()));
        }
        if let Some(expires) = record.expires_at_us
            && expires <= chrono::Utc::now().timestamp_micros()
        {
            return Err(SandboxApiError::NotFound(sandbox_id.to_string()));
        }
        if record.token_hash != sha256_hex(token) {
            return Err(SandboxApiError::Unauthorized);
        }
        Ok(record)
    }

    /// 代理目标解析：取沙箱 Pod IP（授权之后调用）。
    pub async fn pod_ip(&self, record: &SandboxInstanceRecord) -> Result<String, SandboxApiError> {
        let status = self
            .k8s
            .pod_status(&record.pod_name)
            .await
            .map_err(k8s_error)?
            .ok_or_else(|| SandboxApiError::NotFound(record.pod_name.clone()))?;
        status
            .pod_ip
            .ok_or_else(|| SandboxApiError::Unavailable("pod 无 IP".into()))
    }

    // ---------- Volumes（M5） ----------

    pub async fn create_volume(
        &self,
        key: &SandboxApiKeyRecord,
        name: &str,
        size_gib: i32,
    ) -> Result<shared_types::sandbox::SandboxVolumeRecord, SandboxApiError> {
        if name.is_empty() || name.len() > 64 {
            return Err(SandboxApiError::Invalid("volume 名称长度 1-64".into()));
        }
        if size_gib <= 0 || size_gib > self.volume_max_size_gib {
            return Err(SandboxApiError::Invalid(format!(
                "size {}Gi 超限（1-{}Gi）",
                size_gib, self.volume_max_size_gib
            )));
        }
        // 同名检查
        if self
            .store
            .find_volume_by_name(&key.tenant_id, name)
            .await
            .map_err(store_error)?
            .is_some()
        {
            return Err(SandboxApiError::Conflict(format!(
                "volume \"{name}\" 已存在"
            )));
        }
        // 先落行（(tenant,name) 唯一索引承担并发仲裁），再建 PVC——
        // 回收器只删"无行或已软删"的 PVC，这个顺序保证新卷不会被对账误删。
        let volume_id = random_id(12);
        let pvc_name = format!("e2b-vol-{volume_id}");
        let now = chrono::Utc::now().timestamp_micros();
        let record = shared_types::sandbox::SandboxVolumeRecord {
            volume_id: volume_id.clone(),
            tenant_id: key.tenant_id.clone(),
            key_id: key.key_id.clone(),
            name: name.to_string(),
            pvc_name: pvc_name.clone(),
            storage_class: self.volume_storage_class.clone(),
            size_gib,
            created_at_us: now,
            updated_at_us: now,
            deleted_at_us: None,
        };
        let record = self
            .store
            .insert_volume(&record)
            .await
            .map_err(store_error)?;
        if let Err(error) = self
            .k8s
            .create_pvc(&pvc_name, &self.volume_storage_class, size_gib)
            .await
        {
            // 建卷失败回滚：软删行 + 清理可能半建的 PVC。
            let _unused = self.store.mark_volume_deleted(&volume_id).await;
            let _unused = self.k8s.delete_pvc(&pvc_name).await;
            return Err(k8s_error(error));
        }
        Ok(record)
    }

    pub async fn list_volumes(
        &self,
        key: &SandboxApiKeyRecord,
    ) -> Result<Vec<shared_types::sandbox::SandboxVolumeRecord>, SandboxApiError> {
        self.store
            .list_volumes(&key.tenant_id)
            .await
            .map_err(store_error)
    }

    pub async fn delete_volume(
        &self,
        key: &SandboxApiKeyRecord,
        volume_id: &str,
    ) -> Result<(), SandboxApiError> {
        let record = self
            .store
            .get_volume(volume_id)
            .await
            .map_err(store_error)?
            .ok_or_else(|| SandboxApiError::NotFound(volume_id.to_string()))?;
        if record.tenant_id != key.tenant_id {
            return Err(SandboxApiError::NotFound(volume_id.to_string()));
        }
        if record.deleted_at_us.is_some() {
            return Ok(()); // 幂等
        }
        // 先软删（防新沙箱挂载），再删 PVC。
        self.store
            .mark_volume_deleted(volume_id)
            .await
            .map_err(store_error)?;
        let _unused = self.k8s.delete_pvc(&record.pvc_name).await; // PVC 删除失败不阻塞（回收器兜底）
        tracing::info!(volume_id = %record.volume_id, "e2b volume deleted");
        Ok(())
    }

    // ---------- 温池（M4） ----------

    /// 尝试从温池认领一个实例。成功返回 Some(CreateOutcome)；池空/被并发抢先
    /// 返回 None 走冷启动。认领成功后的任何失败都会 kill 该实例（不留孤儿）。
    async fn try_claim_warm(
        &self,
        key: &SandboxApiKeyRecord,
        params: &CreateSandboxParams,
    ) -> Result<Option<CreateOutcome>, SandboxApiError> {
        // 先做与冷路径一致的入参校验（模板可用 + TTL 合法），再找候选。
        let Some(spec) = self
            .store
            .get_template_spec(&params.template_id)
            .await
            .map_err(store_error)?
        else {
            return Ok(None);
        };
        let ttl_seconds = params.timeout_seconds.unwrap_or(self.default_ttl_seconds);
        if ttl_seconds <= 0 {
            return Err(SandboxApiError::InvalidCode(
                shared_types::error_codes::ERR_E2B_TIMEOUT_INVALID,
            ));
        }
        let ttl_cap = self.max_ttl_seconds.min(spec.ttl_max_seconds);
        if ttl_seconds > ttl_cap {
            tracing::info!(ttl_seconds, ttl_cap, "e2b timeout exceeds cap");
            return Err(SandboxApiError::InvalidCode(
                shared_types::error_codes::ERR_E2B_TIMEOUT_EXCEEDS_CAP,
            ));
        }

        let candidates = self.store.list_warm_ready().await.map_err(store_error)?;
        // 候选必须匹配当前修订：模板升级（rev+1）后旧修订的温池实例不得再
        // 认领——它们由闲置过期自然排空，新修订池由补池器按新水位补建。
        let Some(mut record) = candidates
            .into_iter()
            .find(|r| r.template_id == params.template_id && r.template_revision == spec.revision)
        else {
            return Ok(None);
        };

        let token = random_token();
        let token_hash = sha256_hex(&token);
        let encrypted_token = self.encrypt_token(&token)?;
        let env_json = serde_json::to_string(&params.env)
            .map_err(|e| SandboxApiError::Unavailable(e.to_string()))?;
        // hard_deadline 与冷路径同公式，但从认领时刻起算（Pod deadline 同步重置）。
        let now = chrono::Utc::now().timestamp_micros();
        let lifetime_seconds =
            spec.ttl_max_seconds + self.create_ready_timeout.as_secs() as i64 + 120;
        let hard_deadline_us = now + lifetime_seconds * 1_000_000;

        // 原子认领；CAS 冲突 = 被并发 create 抢先 → 回落冷启动而非报 409。
        record = match self
            .store
            .claim_warm_instance(shared_types::sandbox::ClaimWarmInstance {
                sandbox_id: &record.sandbox_id,
                expected_revision: record.instance_revision,
                tenant_id: &key.tenant_id,
                key_id: &key.key_id,
                token_hash: &token_hash,
                encrypted_token: &encrypted_token,
                env_json: &env_json,
                metadata_json: params.metadata.as_deref(),
                idempotency_key: params.idempotency_key.as_deref(),
                hard_deadline_us,
            })
            .await
        {
            Ok(record) => record,
            Err(SandboxStoreError::Conflict(_)) => {
                tracing::debug!("e2b warm claim CAS 冲突，回落冷启动");
                return Ok(None);
            }
            Err(error) => return Err(store_error(error)),
        };
        tracing::info!(sandbox_id = %record.sandbox_id, "e2b warm claim: 从温池认领");

        let claimed = async {
            let status = self
                .k8s
                .pod_status(&record.pod_name)
                .await
                .map_err(k8s_error)?
                .ok_or_else(|| SandboxApiError::Unavailable("温池 Pod 消失".into()))?;
            let pod_ip = status
                .pod_ip
                .clone()
                .ok_or_else(|| SandboxApiError::Unavailable("温池 Pod 无 IP".into()))?;
            // K8s 的 activeDeadlineSeconds 只允许下调；deadline 计时从 Pod startTime
            // 起算。认领要的"认领时刻 + lifetime"换算为绝对秒数 =
            // lifetime + 已闲置时长（spawn 时给了 idle_ttl+lifetime 全程余量，
            // 此值恒 ≤ spawn 值，合法下调）。
            let patch_deadline = match status.start_time_unix {
                Some(started) => {
                    let elapsed = ((now - started * 1_000_000) / 1_000_000).max(0);
                    lifetime_seconds + elapsed
                }
                None => lifetime_seconds,
            };
            self.k8s
                .patch_pod_deadline(&record.pod_name, patch_deadline)
                .await
                .map_err(k8s_error)?;
            let mut env_map = spec.default_env.clone();
            env_map.extend(params.env.iter().cloned());
            self.envd_init(&pod_ip, &token, &spec, &env_map)
                .await
                .map_err(SandboxApiError::Unavailable)?;
            let ready_at = chrono::Utc::now().timestamp_micros();
            record = self
                .store
                .mark_ready(
                    &record.sandbox_id,
                    record.instance_revision,
                    status.pod_uid.as_deref(),
                    ready_at,
                    ready_at + ttl_seconds * 1_000_000,
                )
                .await
                .map_err(store_error)?;
            Ok::<_, SandboxApiError>(())
        }
        .await;

        match claimed {
            Ok(()) => Ok(Some(CreateOutcome { record, token })),
            Err(error) => {
                // 认领后的失败必须回收：实例已带租户数据且置了 Initializing，
                // 不 kill 会变成无 expires 的僵尸行。
                tracing::warn!(
                    sandbox_id = %record.sandbox_id,
                    error = %error.to_string(),
                    "e2b warm claim 失败，回收实例"
                );
                let _unused = self.kill_by_record(&record, KillReason::Explicit).await;
                Err(error)
            }
        }
    }

    /// 温池补池器（后台任务体）：保持每模板的 WarmReady 数量 ≥ warm_target。
    pub async fn run_pool_replenisher(
        self: Arc<Self>,
        mut shutdown: tokio::sync::broadcast::Receiver<()>,
        interval_seconds: u64,
    ) {
        if !self.warm_pool_enabled {
            tracing::info!("e2b warm pool disabled, replenisher not started");
            return;
        }
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_seconds.max(10)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.recv() => break,
            }
            // 遍历模板，检查温池水位
            let templates = match self.store.list_templates().await {
                Ok(list) => list.into_iter().filter(|t| t.enabled).collect::<Vec<_>>(),
                Err(error) => {
                    tracing::warn!(error = %error, "warm pool: 模板列表获取失败");
                    continue;
                }
            };
            // 每轮一次全量温池水位扫描，按模板分组（替代逐模板全表扫）。
            let warm_all = match self.store.list_warm_ready().await {
                Ok(list) => list,
                Err(error) => {
                    tracing::warn!(error = %error, "warm pool: 温池水位扫描失败");
                    continue;
                }
            };
            for template in templates {
                // 数据库错误与"模板没配 spec"是两件事：前者会让这个模板本轮
                // 静默不补池（水位悄悄补不满，只能靠事后发现），必须单独 warn。
                let spec = match self.store.get_template_spec(&template.template_id).await {
                    Ok(Some(spec)) => spec,
                    Ok(None) => continue,
                    Err(error) => {
                        tracing::warn!(
                            template = %template.template_id,
                            error = %error,
                            "warm pool: 模板 spec 获取失败"
                        );
                        continue;
                    }
                };
                if spec.warm_target <= 0 {
                    continue;
                }
                let warm_count = warm_all
                    .iter()
                    // 水位按 (template_id, revision)：旧修订实例不计入新修订水位，
                    // 保证模板升级后新修订池立即开始补建。
                    .filter(|r| {
                        r.template_id == template.template_id
                            && r.template_revision == spec.revision
                    })
                    .count();
                if warm_count >= spec.warm_target as usize {
                    continue;
                }
                // 补一个温池实例
                tracing::info!(template = %template.template_id, warm_count, target = spec.warm_target, "warm pool: 补池");
                if let Err(error) = self.spawn_warm_instance(&spec).await {
                    tracing::warn!(template = %template.template_id, error = %error.to_string(), "warm pool: 补池失败");
                }
            }
        }
        tracing::info!("e2b warm pool replenisher stopped");
    }

    /// 创建一个温池实例（无租户数据，token 为占位符）。
    /// 两阶段：先 Provisioning（带闲置 expires，可被恢复/回收器收敛），Pod Running
    /// 后才转 WarmReady 进入可认领池；任一步失败即终态化，绝不留僵尸 WarmReady 行。
    async fn spawn_warm_instance(&self, spec: &SandboxTemplateSpec) -> Result<(), SandboxApiError> {
        let sandbox_id = random_id(12);
        let pod_name = format!("e2b-warm-{sandbox_id}");
        let placeholder_token = random_token();
        let encrypted = self.encrypt_token(&placeholder_token)?;
        let now = chrono::Utc::now().timestamp_micros();
        let record = SandboxInstanceRecord {
            sandbox_id: sandbox_id.clone(),
            tenant_id: "_warm_pool".to_string(),
            key_id: WARM_POOL_KEY_ID.to_string(),
            template_id: spec.template_id.clone(),
            template_revision: spec.revision,
            image: spec.image.clone(),
            image_digest: spec.image_digest.clone(),
            envd_version: spec.envd_version.clone(),
            runtime_class: spec.runtime_class.clone(),
            cpu_millicores: spec.cpu_millicores,
            memory_mib: spec.memory_mib,
            // Provisioning 而非 WarmReady：Pod 未 Running 前不可认领。
            desired_state: SandboxInstanceState::Provisioning,
            observed_state: None,
            instance_revision: 1,
            namespace: self.k8s.namespace().to_string(),
            pod_name,
            pod_uid: None,
            token_hash: sha256_hex(&placeholder_token),
            encrypted_token: Some(encrypted),
            token_generation: 1,
            env: Vec::new(),
            metadata: None,
            idempotency_key: None,
            failure_reason: None,
            created_at_us: now,
            updated_at_us: now,
            ready_at_us: None,
            // 未认领到期由回收器收敛（list_expired 含 warm_ready）。
            expires_at_us: Some(now + self.warm_idle_ttl_seconds * 1_000_000),
            hard_deadline_us: None,
            deleted_at_us: None,
        };
        let record = self
            .store
            .insert_instance(&record)
            .await
            .map_err(store_error)?;
        let pod_params = SandboxPodParams {
            name: record.pod_name.clone(),
            image: record.image.clone(),
            runtime_class: record.runtime_class.clone(),
            cpu_millicores: record.cpu_millicores,
            memory_mib: record.memory_mib,
            envd_port: ENVD_PORT,
            // K8s 只允许下调 activeDeadline：spawn 就给"闲置窗口 + 完整可认领
            // 生命周期"，认领时换算下调；未认领的兜底回收靠 record expires（更早）。
            max_lifetime_seconds: self.warm_idle_ttl_seconds
                + spec.ttl_max_seconds
                + self.create_ready_timeout.as_secs() as i64
                + 120,
            env: Vec::new(),
            volume_mounts: Vec::new(),
        };
        match self.k8s.create_pod(&pod_params).await {
            Ok(_) => {}
            Err(error) => {
                let _unused = self.k8s.delete_pod(&record.pod_name).await;
                let _unused = self
                    .store
                    .finish(
                        &sandbox_id,
                        record.instance_revision,
                        SandboxInstanceState::Failed,
                        Some(&error.to_string()),
                    )
                    .await;
                return Err(k8s_error(error));
            }
        }
        match self
            .k8s
            .wait_ready(&record.pod_name, self.create_ready_timeout)
            .await
        {
            Ok(_) => {}
            Err(error) => {
                let _unused = self.k8s.delete_pod(&record.pod_name).await;
                let _unused = self
                    .store
                    .finish(
                        &sandbox_id,
                        record.instance_revision,
                        SandboxInstanceState::Failed,
                        Some(&error.to_string()),
                    )
                    .await;
                return Err(k8s_error(error));
            }
        }
        self.store
            .transition_state(
                &sandbox_id,
                record.instance_revision,
                SandboxInstanceState::WarmReady,
                None,
                Some("Running"),
            )
            .await
            .map_err(store_error)?;
        tracing::info!(sandbox_id = %sandbox_id, template = %spec.template_id, "warm pool: 实例就绪");
        Ok(())
    }

    /// 超时回收循环（tokio 任务体）：到期实例与待清理实例的收敛。
    pub async fn run_reaper(
        self: Arc<Self>,
        mut shutdown: tokio::sync::broadcast::Receiver<()>,
        interval_seconds: u64,
    ) {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_seconds.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.recv() => break,
            }
            let now = chrono::Utc::now().timestamp_micros();
            let expired = match self.store.list_expired(now).await {
                Ok(list) => list,
                Err(error) => {
                    tracing::warn!("e2b reaper list_expired: {error}");
                    continue;
                }
            };
            for record in expired {
                tracing::info!(sandbox_id = %record.sandbox_id, "e2b sandbox expired");
                if let Err(error) = self.kill_by_record(&record, KillReason::ReapExpired).await {
                    tracing::warn!(sandbox_id = %record.sandbox_id, error = %error, "e2b reaper kill 失败，下轮重试");
                }
            }
            // Terminating 卡死兜底（复审 P2）：kill 第一步落库后进程中断，
            // 行残留旧 expires（甚至 NULL）→ list_expired 按 expires 过滤漏扫。
            // 超过 10 分钟仍停在 Terminating 的行强制对账收敛。
            const STUCK_TERMINATING_GRACE_US: i64 = 600 * 1_000_000;
            if let Ok(all_non_terminal) = self.store.list_all_non_terminal().await {
                for record in all_non_terminal {
                    if record.desired_state != SandboxInstanceState::Terminating {
                        continue;
                    }
                    if now - record.updated_at_us < STUCK_TERMINATING_GRACE_US {
                        continue;
                    }
                    tracing::warn!(
                        sandbox_id = %record.sandbox_id,
                        updated_at = record.updated_at_us,
                        "e2b reaper: Terminating 卡死超宽限，强制收敛"
                    );
                    // KillReason 用 Recover 而非 ReapExpired: 卡死行的 expires
                    // 可能是 NULL/未来值(kill 两步间中断的历史残留),
                    // ReapExpired 的到期复核会直接放弃——Recover 只复核非终态
                    // (复审第四轮 P2), 10min 卡死宽限本身就是处置凭据。
                    if let Err(error) = self.kill_by_record(&record, KillReason::Recover).await {
                        tracing::warn!(sandbox_id = %record.sandbox_id, error = %error, "e2b reaper 卡死收敛失败，下轮重试");
                    }
                }
            }
            let pending = match self.store.list_cleanup_pending().await {
                Ok(list) => list,
                Err(error) => {
                    tracing::warn!("e2b reaper list_cleanup_pending: {error}");
                    continue;
                }
            };
            for record in pending {
                match self.k8s.delete_pod(&record.pod_name).await {
                    Ok(_) => {
                        let _unused = self
                            .store
                            .finish(
                                &record.sandbox_id,
                                record.instance_revision,
                                SandboxInstanceState::Deleted,
                                Some("cleanup reconciled"),
                            )
                            .await;
                    }
                    Err(error) => {
                        tracing::warn!(sandbox_id = %record.sandbox_id, error = %error.to_string(), "e2b reaper 清理删除失败，保留待清理");
                    }
                }
            }
            // PVC 对账（P1-5b）：行已软删或不存在的卷，其 PVC 兜底回收——
            // 覆盖 delete_volume 的 PVC 删除失败与 create_volume 的半建残留。
            // 查询失败时保守跳过（本轮不删任何 PVC）。
            // 反向：行活跃但 PVC 长期缺失（落行与建 PVC 之间中断）——软删行
            // 释放占名，防永久占名空卷。
            let (pvc_names, active_volumes) = match (
                self.k8s.list_volume_pvcs().await,
                self.store.list_all_active_volumes().await,
            ) {
                (Ok(pvcs), Ok(volumes)) => (pvcs, volumes),
                (pvc_result, volume_result) => {
                    if let Err(error) = pvc_result {
                        tracing::warn!(error = %error.to_string(), "e2b reaper PVC 列举失败");
                    }
                    if let Err(error) = volume_result {
                        tracing::warn!(error = %error.to_string(), "e2b reaper 卷行列举失败");
                    }
                    continue;
                }
            };
            for pvc_name in &pvc_names {
                let Some(volume_id) = pvc_name.strip_prefix("e2b-vol-") else {
                    continue;
                };
                let orphaned = match self.store.get_volume(volume_id).await {
                    Ok(None) => true,
                    Ok(Some(volume)) => volume.deleted_at_us.is_some(),
                    Err(error) => {
                        tracing::warn!(%pvc_name, error = %error, "e2b reaper PVC 对账查询失败，保守跳过");
                        false
                    }
                };
                if !orphaned {
                    continue;
                }
                match self.k8s.delete_pvc(pvc_name).await {
                    Ok(_) => {
                        tracing::info!(%pvc_name, "e2b reaper: 回收孤儿 PVC");
                    }
                    Err(error) => {
                        tracing::warn!(%pvc_name, error = %error.to_string(), "e2b reaper PVC 回收失败，下轮重试");
                    }
                }
            }
            const VOLUME_PVC_GRACE_SECONDS: i64 = 300;
            let pvc_set: std::collections::HashSet<&String> = pvc_names.iter().collect();
            let now_us = chrono::Utc::now().timestamp_micros();
            for volume in &active_volumes {
                if pvc_set.contains(&volume.pvc_name) {
                    continue;
                }
                if now_us - volume.created_at_us < VOLUME_PVC_GRACE_SECONDS * 1_000_000 {
                    continue; // 宽限期内（创建窗口），不动
                }
                tracing::warn!(
                    volume_id = %volume.volume_id,
                    pvc = %volume.pvc_name,
                    "e2b reaper: 卷行活跃但 PVC 缺失超宽限期，软删释放占名"
                );
                let _unused = self.store.mark_volume_deleted(&volume.volume_id).await;
            }
        }
        tracing::info!("e2b reaper stopped");
    }
}

fn ensure_tenant(
    record: &SandboxInstanceRecord,
    key: &SandboxApiKeyRecord,
) -> Result<(), SandboxApiError> {
    if record.tenant_id != key.tenant_id {
        // 跨租户访问按不存在处理，不泄露存在性。
        return Err(SandboxApiError::NotFound(record.sandbox_id.clone()));
    }
    Ok(())
}

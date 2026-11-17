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
    sha256_hex, CreateSandboxParams, SandboxApiKeyRecord, SandboxInstanceRecord,
    SandboxInstanceState, SandboxStore, SandboxStoreError, SandboxTemplateSpec,
};

use crate::config::E2bSandboxConfig;
use docker_manager::runtime::k8s_sandbox::{
    SandboxK8s, SandboxK8sError, SandboxPodParams,
};

pub const ENVD_PORT: i32 = 49983;

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
        let key = aes_gcm::Key::<Aes256Gcm>::from_slice(&key_bytes);
        if key_bytes.len() != 32 {
            return Err(format!(
                "e2b_sandbox token_key_hex 长度错误: {} 字节（需要 32）",
                key_bytes.len()
            ));
        }
        Ok(Self {
            store,
            k8s,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(|error| format!("e2b_sandbox http client: {error}"))?,
            default_ttl_seconds: config.default_ttl_seconds,
            max_ttl_seconds: config.max_ttl_seconds,
            create_ready_timeout: Duration::from_secs(config.create_ready_timeout_seconds),
            envd_init_retries: config.envd_init_retries.max(1),
            cipher: Aes256Gcm::new(key),
            public_domain: config.public_domain.clone(),
        })
    }

    /// 配置驱动的幂等种子（api key 哈希 + 模板注册表）。
    pub async fn seed_registry(&self, config: &E2bSandboxConfig) -> Result<(), SandboxApiError> {
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
                runtime_class: template.runtime_class.clone().filter(|value| !value.is_empty()),
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

    pub async fn authenticate(&self, provided_key: &str) -> Result<SandboxApiKeyRecord, SandboxApiError> {
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
        Ok(base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &packed))
    }

    pub fn decrypt_token(&self, record: &SandboxInstanceRecord) -> Result<String, SandboxApiError> {
        let packed = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, record.encrypted_token.as_deref().unwrap_or_default())
            .map_err(|error| SandboxApiError::Unavailable(format!("token b64: {error}")))?;
        if packed.len() < 12 {
            return Err(SandboxApiError::Unavailable("token 密文损坏".into()));
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
        let spec = self
            .store
            .get_template_spec(&params.template_id)
            .await
            .map_err(store_error)?
            .ok_or_else(|| SandboxApiError::Invalid(format!("template {} 不可用", params.template_id)))?;
        let ttl_seconds = params.timeout_seconds.unwrap_or(self.default_ttl_seconds);
        if ttl_seconds <= 0 {
            return Err(SandboxApiError::Invalid("timeout 必须为正数秒".into()));
        }
        let ttl_cap = self.max_ttl_seconds.min(spec.ttl_max_seconds);
        if ttl_seconds > ttl_cap {
            return Err(SandboxApiError::Invalid(format!(
                "timeout {ttl_seconds}s 超过上限 {ttl_cap}s"
            )));
        }

        let sandbox_id = random_id(12);
        let pod_name = format!("e2b-{sandbox_id}");
        let token = random_token();
        let token_hash = sha256_hex(&token);
        let encrypted_token = self.encrypt_token(&token)?;

        let now = chrono::Utc::now().timestamp_micros();
        // hard_deadline：不可续期硬顶 = 创建时刻 + 模板最大存活 + 创建等待余量。
        let hard_deadline_us = now
            + (spec.ttl_max_seconds
                + self.create_ready_timeout.as_secs() as i64
                + 120)
                * 1_000_000;

        let mut record = SandboxInstanceRecord {
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
            failure_reason: None,
            created_at_us: now,
            updated_at_us: now,
            ready_at_us: None,
            expires_at_us: None,
            hard_deadline_us: Some(hard_deadline_us),
            deleted_at_us: None,
        };
        record = self.store.insert_instance(&record).await.map_err(store_error)?;

        // 外部副作用：Pod 创建 + 等待 Ready。
        let pod_params = SandboxPodParams {
            name: record.pod_name.clone(),
            image: record.image.clone(),
            runtime_class: record.runtime_class.clone(),
            cpu_millicores: record.cpu_millicores,
            memory_mib: record.memory_mib,
            envd_port: ENVD_PORT,
            max_lifetime_seconds: spec.ttl_max_seconds + self.create_ready_timeout.as_secs() as i64 + 120,
            env: Vec::new(),
        };
        if let Err(error) = self.k8s.create_pod(&pod_params).await {
            return self
                .fail_create(&record, &format!("create pod: {error}"))
                .await;
        }
        let status = match self.k8s.wait_ready(&record.pod_name, self.create_ready_timeout).await {
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
                return self.fail_create(&record, &format!("envd init: {error}")).await;
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
                let _ = self
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
                let _ = self
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
        Err(SandboxApiError::Unavailable(format!("create failed: {reason}")))
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
                    return Err(format!("envd /init 4xx: {}", response.status()))
                }
                Ok(response) => {
                    let status = response.status();
                    if attempt >= self.envd_init_retries {
                        return Err(format!("envd /init {status} 重试 {} 次后放弃", self.envd_init_retries));
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

    /// 幂等 kill：终态重复调用返回成功。
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
        let mut record = self
            .store
            .transition_state(
                sandbox_id,
                record.instance_revision,
                SandboxInstanceState::Terminating,
                None,
                None,
            )
            .await
            .map_err(store_error)?;
        match self.k8s.delete_pod(&record.pod_name).await {
            Ok(_) => {}
            Err(error) => {
                // 删除失败：保留待清理，等待回收器对账。
                record = self
                    .store
                    .transition_state(
                        sandbox_id,
                        record.instance_revision,
                        SandboxInstanceState::CleanupPending,
                        Some(&format!("pod delete: {error}")),
                        None,
                    )
                    .await
                    .map_err(store_error)?;
            }
        }
        let _ = self
            .store
            .finish(
                sandbox_id,
                record.instance_revision,
                SandboxInstanceState::Deleted,
                None,
            )
            .await;
        Ok(())
    }

    pub async fn set_timeout(
        &self,
        key: &SandboxApiKeyRecord,
        sandbox_id: &str,
        timeout_seconds: i64,
    ) -> Result<(), SandboxApiError> {
        let record = self.get(key, sandbox_id).await?;
        if record.desired_state != SandboxInstanceState::Ready {
            return Err(SandboxApiError::Invalid("仅 Ready 状态可续期".into()));
        }
        if timeout_seconds <= 0 {
            return Err(SandboxApiError::Invalid("timeout 必须为正数秒".into()));
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
        let now = chrono::Utc::now().timestamp_micros();
        let new_expires = now + timeout_seconds * 1_000_000;
        if let Some(hard) = record.hard_deadline_us {
            if new_expires > hard {
                return Err(SandboxApiError::Invalid(format!(
                    "续期越过硬顶（hard_deadline）"
                )));
            }
        }
        self.store
            .set_expires(sandbox_id, record.instance_revision, new_expires)
            .await
            .map_err(store_error)?;
        Ok(())
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
        if let Some(expires) = record.expires_at_us {
            if expires <= chrono::Utc::now().timestamp_micros() {
                return Err(SandboxApiError::NotFound(sandbox_id.to_string()));
            }
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

    /// 超时回收循环（tokio 任务体）：到期实例与待清理实例的收敛。
    pub async fn run_reaper(self: Arc<Self>, mut shutdown: tokio::sync::broadcast::Receiver<()>, interval_seconds: u64) {
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
                if let Err(error) = self.kill_by_record(&record).await {
                    tracing::warn!(sandbox_id = %record.sandbox_id, error = %error, "e2b reaper kill 失败，下轮重试");
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
                        let _ = self
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
        }
        tracing::info!("e2b reaper stopped");
    }

    async fn kill_by_record(&self, record: &SandboxInstanceRecord) -> Result<(), SandboxApiError> {
        let mut record = self
            .store
            .transition_state(
                &record.sandbox_id,
                record.instance_revision,
                SandboxInstanceState::Terminating,
                None,
                None,
            )
            .await
            .map_err(store_error)?;
        match self.k8s.delete_pod(&record.pod_name).await {
            Ok(_) => {}
            Err(error) => {
                record = self
                    .store
                    .transition_state(
                        &record.sandbox_id,
                        record.instance_revision,
                        SandboxInstanceState::CleanupPending,
                        Some(&format!("pod delete: {error}")),
                        None,
                    )
                    .await
                    .map_err(store_error)?;
            }
        }
        let _ = self
            .store
            .finish(
                &record.sandbox_id,
                record.instance_revision,
                SandboxInstanceState::Deleted,
                Some("expired"),
            )
            .await;
        Ok(())
    }
}

fn ensure_tenant(record: &SandboxInstanceRecord, key: &SandboxApiKeyRecord) -> Result<(), SandboxApiError> {
    if record.tenant_id != key.tenant_id {
        // 跨租户访问按不存在处理，不泄露存在性。
        return Err(SandboxApiError::NotFound(record.sandbox_id.clone()));
    }
    Ok(())
}

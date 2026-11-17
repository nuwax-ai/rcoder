//! E2B 兼容沙箱的 PG 存储（Component::Sandbox）。
//!
//! 读走 Toasty 模型（filter_by_*），CAS 更新与到期扫描走裸语句；
//! 事务纪律沿用 preview store：变更严格在提交后返回，未知结果由
//! 调用方按 operation 语义对账（M3 引入台账前的过渡约束）。
use crate::db::{
    models::{SandboxApiKey, SandboxInstance, SandboxTemplate, SandboxTemplateRevision, SandboxVolume},
    owner::DatabaseOwner,
    schema::Component,
};
use futures::future::BoxFuture;
use shared_types::sandbox::{
    SandboxApiKeyRecord, SandboxInstanceRecord, SandboxInstanceState,
    SandboxStore, SandboxStoreError, SandboxTemplateSpec, SandboxTemplateSummary,
    SandboxVolumeRecord,
};
use toasty::Executor;
use toasty_core::schema::db::Type;

pub struct PgSandboxStore {
    owner: DatabaseOwner,
}

fn unavailable<E: std::fmt::Display>(error: E) -> SandboxStoreError {
    SandboxStoreError::Unavailable(error.to_string())
}

fn now_us() -> i64 {
    chrono::Utc::now().timestamp_micros()
}

impl PgSandboxStore {
    pub async fn connect(config: &crate::config::PostgresConfig) -> Result<Self, SandboxStoreError> {
        Ok(Self {
            owner: crate::db::postgres::open(config, vec![Component::Sandbox])
                .await
                .map_err(unavailable)?,
        })
    }
    pub async fn close(&self) -> Result<(), SandboxStoreError> {
        self.owner.shutdown().await.map_err(unavailable)
    }
    async fn run<T, F>(&self, body: F) -> Result<T, SandboxStoreError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut dyn Executor) -> BoxFuture<'a, Result<T, SandboxStoreError>>
            + Send
            + 'static,
    {
        self.owner
            .execute(move |mut db| async move {
                let mut tx = match db.transaction().await {
                    Ok(tx) => tx,
                    Err(error) => return Ok(Err(SandboxStoreError::Unavailable(error.to_string()))),
                };
                match body(&mut tx).await {
                    Ok(value) => match tx.commit().await {
                        Ok(()) => Ok(Ok(value)),
                        Err(error) => Ok(Err(SandboxStoreError::Unavailable(error.to_string()))),
                    },
                    Err(error) => {
                        if let Err(rollback) = tx.rollback().await {
                            return Ok(Err(SandboxStoreError::Unavailable(format!(
                                "sandbox transaction failed ({error}); rollback failed ({rollback}); outcome requires verification"
                            ))));
                        }
                        Ok(Err(error))
                    }
                }
            })
            .await
            .map_err(unavailable)?
    }
}

// ---------- 记录映射（模型 ↔ shared_types） ----------

fn parse_env_json(json: &Option<String>) -> Result<Vec<(String, String)>, SandboxStoreError> {
    match json {
        None => Ok(Vec::new()),
        Some(raw) => serde_json::from_str::<Vec<(String, String)>>(raw)
            .map_err(|error| SandboxStoreError::Corrupt(format!("env json: {error}"))),
    }
}

fn template_spec(row: SandboxTemplateRevision) -> Result<SandboxTemplateSpec, SandboxStoreError> {
    Ok(SandboxTemplateSpec {
        template_id: row.template_id,
        revision: row.revision,
        image: row.image,
        image_digest: row.image_digest,
        envd_version: row.envd_version,
        runtime_class: row.runtime_class,
        cpu_millicores: row.cpu_millicores,
        memory_mib: row.memory_mib,
        default_user: row.default_user,
        default_workdir: row.default_workdir,
        default_env: parse_env_json(&row.default_env_json)?,
        ttl_max_seconds: row.ttl_max_seconds,
        warm_target: row.warm_target,
    })
}

fn api_key_record(row: SandboxApiKey) -> SandboxApiKeyRecord {
    SandboxApiKeyRecord {
        key_id: row.key_id,
        tenant_id: row.tenant_id,
        public_prefix: row.public_prefix,
        key_hash: row.key_hash,
        scopes: row.scopes,
        expires_at_us: row.expires_at_us,
        revoked_at_us: row.revoked_at_us,
    }
}

fn state_from_str(raw: &str) -> Result<SandboxInstanceState, SandboxStoreError> {
    match raw {
        "provisioning" => Ok(SandboxInstanceState::Provisioning),
        "initializing" => Ok(SandboxInstanceState::Initializing),
        "ready" => Ok(SandboxInstanceState::Ready),
        "terminating" => Ok(SandboxInstanceState::Terminating),
        "cleanup_pending" => Ok(SandboxInstanceState::CleanupPending),
        "deleted" => Ok(SandboxInstanceState::Deleted),
        "failed" => Ok(SandboxInstanceState::Failed),
        other => Err(SandboxStoreError::Corrupt(format!("state {other}"))),
    }
}

fn instance_record(row: SandboxInstance) -> Result<SandboxInstanceRecord, SandboxStoreError> {
    Ok(SandboxInstanceRecord {
        sandbox_id: row.sandbox_id,
        tenant_id: row.tenant_id,
        key_id: row.key_id,
        template_id: row.template_id,
        template_revision: row.template_revision,
        image: row.image,
        image_digest: row.image_digest,
        envd_version: row.envd_version,
        runtime_class: row.runtime_class,
        cpu_millicores: row.cpu_millicores,
        memory_mib: row.memory_mib,
        desired_state: state_from_str(&row.desired_state)?,
        observed_state: row.observed_state,
        instance_revision: row.instance_revision,
        namespace: row.namespace,
        pod_name: row.pod_name,
        pod_uid: row.pod_uid,
        token_hash: row.token_hash,
        encrypted_token: row.encrypted_token,
        token_generation: row.token_generation,
        env: parse_env_json(&row.env_json)?,
        metadata: row.metadata_json,
        failure_reason: row.failure_reason,
        created_at_us: row.created_at_us,
        updated_at_us: row.updated_at_us,
        ready_at_us: row.ready_at_us,
        expires_at_us: row.expires_at_us,
        hard_deadline_us: row.hard_deadline_us,
        deleted_at_us: row.deleted_at_us,
    })
}

// ---------- SandboxStore 实现 ----------

#[async_trait::async_trait]
impl SandboxStore for PgSandboxStore {
    async fn get_template_spec(
        &self,
        template_id: &str,
    ) -> Result<Option<SandboxTemplateSpec>, SandboxStoreError> {
        let template_id = template_id.to_string();
        self.run(move |tx| {
            Box::pin(async move {
                let Some(head) = SandboxTemplate::filter_by_template_id(&template_id)
                    .first()
                    .exec(tx)
                    .await
                    .map_err(unavailable)?
                else {
                    return Ok(None);
                };
                if !head.enabled {
                    return Ok(None);
                }
                let revisions = SandboxTemplateRevision::filter_by_template_id(&template_id)
                    .exec(tx)
                    .await
                    .map_err(unavailable)?;
                let row = revisions
                    .into_iter()
                    .find(|row| row.revision == head.current_revision)
                    .ok_or_else(|| {
                        SandboxStoreError::Corrupt(format!(
                            "template {template_id} missing revision {}",
                            head.current_revision
                        ))
                    })?;
                Ok(Some(template_spec(row)?))
            })
        })
        .await
    }

    async fn list_templates(&self) -> Result<Vec<SandboxTemplateSummary>, SandboxStoreError> {
        self.run(move |tx| {
            Box::pin(async move {
                let heads = SandboxTemplate::all()
                    .exec(tx)
                    .await
                    .map_err(unavailable)?;
                Ok(heads
                    .into_iter()
                    .map(|head| SandboxTemplateSummary {
                        template_id: head.template_id,
                        display_name: head.display_name,
                        enabled: head.enabled,
                        current_revision: head.current_revision,
                    })
                    .collect())
            })
        })
        .await
    }

    async fn seed_api_key(&self, record: &SandboxApiKeyRecord) -> Result<(), SandboxStoreError> {
        let record = record.clone();
        self.run(move |tx| {
            Box::pin(async move {
                let count = toasty::sql::statement(
                    "INSERT INTO sandbox_api_keys(key_id,tenant_id,public_prefix,key_hash,scopes,created_at_us,expires_at_us,revoked_at_us) \
                     VALUES($1,$2,$3,$4,$5,$6,NULL,NULL) ON CONFLICT(key_id) DO NOTHING",
                )
                .bind(&record.key_id)
                .bind(&record.tenant_id)
                .bind(&record.public_prefix)
                .bind(&record.key_hash)
                .bind(&record.scopes)
                .bind(now_us())
                .exec(tx)
                .await
                .map_err(unavailable)?;
                let _ = count;
                Ok(())
            })
        })
        .await
    }

    async fn seed_template(
        &self,
        summary: &SandboxTemplateSummary,
        spec: &SandboxTemplateSpec,
    ) -> Result<(), SandboxStoreError> {
        let summary = summary.clone();
        let spec = spec.clone();
        self.run(move |tx| {
            Box::pin(async move {
                let env_json = serde_json::to_string(&spec.default_env)
                    .map_err(|error| SandboxStoreError::Corrupt(error.to_string()))?;
                let now = now_us();
                // 修订行：不可变，(template_id, revision) 冲突即跳过。
                toasty::sql::statement(
                    "INSERT INTO sandbox_template_revisions(template_id,revision,image,image_digest,envd_version,runtime_class,cpu_millicores,memory_mib,default_user,default_workdir,default_env_json,ttl_max_seconds,warm_target,created_at_us) \
                     VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) ON CONFLICT(template_id,revision) DO NOTHING",
                )
                .bind(&spec.template_id)
                .bind(spec.revision)
                .bind(&spec.image)
                .bind_typed(spec.image_digest.as_deref(), Type::Text)
                .bind(&spec.envd_version)
                .bind_typed(spec.runtime_class.as_deref(), Type::Text)
                .bind(spec.cpu_millicores)
                .bind(spec.memory_mib)
                .bind(&spec.default_user)
                .bind(&spec.default_workdir)
                .bind(&env_json)
                .bind(spec.ttl_max_seconds)
                .bind(spec.warm_target)
                .bind(now)
                .exec(tx)
                .await
                .map_err(unavailable)?;
                // 头行：配置是唯一事实源，允许推进 current_revision/enabled。
                toasty::sql::statement(
                    "INSERT INTO sandbox_templates(template_id,current_revision,enabled,display_name,created_at_us,updated_at_us) \
                     VALUES($1,$2,$3,$4,$5,$5) ON CONFLICT(template_id) DO UPDATE SET \
                     current_revision=EXCLUDED.current_revision,enabled=EXCLUDED.enabled,display_name=EXCLUDED.display_name,updated_at_us=EXCLUDED.updated_at_us",
                )
                .bind(&summary.template_id)
                .bind(summary.current_revision)
                .bind(summary.enabled)
                .bind_typed(summary.display_name.as_deref(), Type::Text)
                .bind(now)
                .exec(tx)
                .await
                .map_err(unavailable)?;
                Ok(())
            })
        })
        .await
    }

    async fn find_api_key_by_hash(
        &self,
        key_hash: &str,
    ) -> Result<Option<SandboxApiKeyRecord>, SandboxStoreError> {
        let key_hash = key_hash.to_string();
        self.run(move |tx| {
            Box::pin(async move {
                let row = SandboxApiKey::filter_by_key_hash(&key_hash)
                    .first()
                    .exec(tx)
                    .await
                    .map_err(unavailable)?;
                Ok(row.map(api_key_record))
            })
        })
        .await
    }

    async fn insert_instance(
        &self,
        record: &SandboxInstanceRecord,
    ) -> Result<SandboxInstanceRecord, SandboxStoreError> {
        let record = record.clone();
        self.run(move |tx| {
            Box::pin(async move {
                let env_json = serde_json::to_string(&record.env)
                    .map_err(|error| SandboxStoreError::Corrupt(error.to_string()))?;
                toasty::sql::statement(
                    "INSERT INTO sandbox_instances(sandbox_id,tenant_id,key_id,template_id,template_revision,image,image_digest,envd_version,runtime_class,cpu_millicores,memory_mib,desired_state,observed_state,instance_revision,namespace,pod_name,pod_uid,token_hash,encrypted_token,token_generation,env_json,metadata_json,failure_reason,created_at_us,updated_at_us,ready_at_us,expires_at_us,hard_deadline_us,deleted_at_us) \
                     VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,NULL,1,$13,$14,NULL,$15,$16,1,$17,$18,NULL,$19,$19,NULL,$20,$21,NULL) \
                     ON CONFLICT(sandbox_id) DO NOTHING",
                )
                .bind(&record.sandbox_id)
                .bind(&record.tenant_id)
                .bind(&record.key_id)
                .bind(&record.template_id)
                .bind(record.template_revision)
                .bind(&record.image)
                .bind_typed(record.image_digest.as_deref(), Type::Text)
                .bind(&record.envd_version)
                .bind_typed(record.runtime_class.as_deref(), Type::Text)
                .bind(record.cpu_millicores)
                .bind(record.memory_mib)
                .bind(record.desired_state.as_str())
                .bind(&record.namespace)
                .bind(&record.pod_name)
                .bind(&record.token_hash)
                .bind_typed(record.encrypted_token.as_deref(), Type::Text)
                .bind(&env_json)
                .bind_typed(record.metadata.as_deref(), Type::Text)
                .bind(record.created_at_us)
                .bind_typed(record.expires_at_us, Type::Integer(8))
                .bind_typed(record.hard_deadline_us, Type::Integer(8))
                .exec(tx)
                .await
                .map_err(unavailable)?;
                // 幂等读回（并发同 ID 时读回已存在行）。
                let row = SandboxInstance::filter_by_sandbox_id(&record.sandbox_id)
                    .first()
                    .exec(tx)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(|| {
                        SandboxStoreError::Unavailable(
                            "sandbox insert row vanished in transaction".into(),
                        )
                    })?;
                instance_record(row)
            })
        })
        .await
    }

    async fn get_instance(
        &self,
        sandbox_id: &str,
    ) -> Result<Option<SandboxInstanceRecord>, SandboxStoreError> {
        let sandbox_id = sandbox_id.to_string();
        self.run(move |tx| {
            Box::pin(async move {
                let row = SandboxInstance::filter_by_sandbox_id(&sandbox_id)
                    .first()
                    .exec(tx)
                    .await
                    .map_err(unavailable)?;
                row.map(instance_record).transpose()
            })
        })
        .await
    }

    async fn transition_state(
        &self,
        sandbox_id: &str,
        expected_revision: i64,
        new_state: SandboxInstanceState,
        failure_reason: Option<&str>,
        observed_state: Option<&str>,
    ) -> Result<SandboxInstanceRecord, SandboxStoreError> {
        let sandbox_id = sandbox_id.to_string();
        let state_str = new_state.as_str().to_string();
        let failure_reason = failure_reason.map(str::to_string);
        let observed_state = observed_state.map(str::to_string);
        self.run(move |tx| {
            let sandbox_id = sandbox_id.clone();
            let state_str = state_str.clone();
            let failure_reason = failure_reason.clone();
            let observed_state = observed_state.clone();
            Box::pin(async move {
                let count = toasty::sql::statement(
                    "UPDATE sandbox_instances SET desired_state=$1,failure_reason=$2,observed_state=$3,instance_revision=instance_revision+1,updated_at_us=$4 WHERE sandbox_id=$5 AND instance_revision=$6",
                )
                .bind(&state_str)
                .bind_typed(failure_reason.as_deref(), Type::Text)
                .bind_typed(observed_state.as_deref(), Type::Text)
                .bind(now_us())
                .bind(&sandbox_id)
                .bind(expected_revision)
                .exec(tx)
                .await
                .map_err(unavailable)?;
                if count != 1 {
                    return Err(SandboxStoreError::Conflict(format!(
                        "sandbox {sandbox_id} CAS failed at revision {expected_revision}"
                    )));
                }
                read_back(tx, &sandbox_id).await
            })
        })
        .await
    }

    async fn mark_ready(
        &self,
        sandbox_id: &str,
        expected_revision: i64,
        pod_uid: Option<&str>,
        ready_at_us: i64,
        expires_at_us: i64,
    ) -> Result<SandboxInstanceRecord, SandboxStoreError> {
        let sandbox_id = sandbox_id.to_string();
        let pod_uid = pod_uid.map(str::to_string);
        self.run(move |tx| {
            let sandbox_id = sandbox_id.clone();
            let pod_uid = pod_uid.clone();
            Box::pin(async move {
                let count = toasty::sql::statement(
                    "UPDATE sandbox_instances SET desired_state='ready',pod_uid=$1,ready_at_us=$2,expires_at_us=$3,instance_revision=instance_revision+1,updated_at_us=$4 WHERE sandbox_id=$5 AND instance_revision=$6",
                )
                .bind_typed(pod_uid.as_deref(), Type::Text)
                .bind(ready_at_us)
                .bind(expires_at_us)
                .bind(now_us())
                .bind(&sandbox_id)
                .bind(expected_revision)
                .exec(tx)
                .await
                .map_err(unavailable)?;
                if count != 1 {
                    return Err(SandboxStoreError::Conflict(format!(
                        "sandbox {sandbox_id} CAS failed at revision {expected_revision}"
                    )));
                }
                read_back(tx, &sandbox_id).await
            })
        })
        .await
    }

    async fn set_expires(
        &self,
        sandbox_id: &str,
        expected_revision: i64,
        expires_at_us: i64,
    ) -> Result<SandboxInstanceRecord, SandboxStoreError> {
        let sandbox_id = sandbox_id.to_string();
        self.run(move |tx| {
            let sandbox_id = sandbox_id.clone();
            Box::pin(async move {
                let count = toasty::sql::statement(
                    "UPDATE sandbox_instances SET expires_at_us=$1,instance_revision=instance_revision+1,updated_at_us=$2 WHERE sandbox_id=$3 AND instance_revision=$4",
                )
                .bind(expires_at_us)
                .bind(now_us())
                .bind(&sandbox_id)
                .bind(expected_revision)
                .exec(tx)
                .await
                .map_err(unavailable)?;
                if count != 1 {
                    return Err(SandboxStoreError::Conflict(format!(
                        "sandbox {sandbox_id} CAS failed at revision {expected_revision}"
                    )));
                }
                read_back(tx, &sandbox_id).await
            })
        })
        .await
    }

    async fn finish(
        &self,
        sandbox_id: &str,
        expected_revision: i64,
        state: SandboxInstanceState,
        failure_reason: Option<&str>,
    ) -> Result<SandboxInstanceRecord, SandboxStoreError> {
        let sandbox_id = sandbox_id.to_string();
        let state_str = state.as_str().to_string();
        let failure_reason = failure_reason.map(str::to_string);
        self.run(move |tx| {
            let sandbox_id = sandbox_id.clone();
            let state_str = state_str.clone();
            let failure_reason = failure_reason.clone();
            Box::pin(async move {
                let count = toasty::sql::statement(
                    "UPDATE sandbox_instances SET desired_state=$1,failure_reason=$2,deleted_at_us=$3,instance_revision=instance_revision+1,updated_at_us=$3 WHERE sandbox_id=$4 AND instance_revision=$5",
                )
                .bind(&state_str)
                .bind_typed(failure_reason.as_deref(), Type::Text)
                .bind(now_us())
                .bind(&sandbox_id)
                .bind(expected_revision)
                .exec(tx)
                .await
                .map_err(unavailable)?;
                if count != 1 {
                    return Err(SandboxStoreError::Conflict(format!(
                        "sandbox {sandbox_id} CAS failed at revision {expected_revision}"
                    )));
                }
                read_back(tx, &sandbox_id).await
            })
        })
        .await
    }

    async fn list_instances(
        &self,
        tenant_id: &str,
        before: Option<(i64, &str)>,
        limit: usize,
    ) -> Result<Vec<SandboxInstanceRecord>, SandboxStoreError> {
        let tenant_id = tenant_id.to_string();
        let before = before.map(|(ts, id)| (ts, id.to_string()));
        self.run(move |tx| {
            let tenant_id = tenant_id.clone();
            let before = before.clone();
            Box::pin(async move {
                // M1：租户内全量读 + 内存游标（规模受限的过渡实现；M3 引入 SQL 游标）。
                let rows = SandboxInstance::filter_by_tenant_id(&tenant_id)
                    .exec(tx)
                    .await
                    .map_err(unavailable)?;
                let mut records = rows
                    .into_iter()
                    .map(instance_record)
                    .collect::<Result<Vec<_>, _>>()?;
                records.sort_by(|a, b| {
                    b.created_at_us
                        .cmp(&a.created_at_us)
                        .then(b.sandbox_id.cmp(&a.sandbox_id))
                });
                let start = match &before {
                    Some((ts, id)) => records
                        .iter()
                        .position(|record| record.created_at_us == *ts && &record.sandbox_id == id)
                        .map(|position| position + 1)
                        .unwrap_or(0),
                    None => 0,
                };
                Ok(records
                    .into_iter()
                    .skip(start)
                    .take(limit)
                    .collect::<Vec<_>>())
            })
        })
        .await
    }

    async fn list_expired(&self, now_us: i64) -> Result<Vec<SandboxInstanceRecord>, SandboxStoreError> {
        self.run(move |tx| {
            Box::pin(async move {
                // 到期实例只可能处于 Ready（Provisioning/Initializing 尚无 expires）。
                let rows = SandboxInstance::filter_by_desired_state("ready")
                    .exec(tx)
                    .await
                    .map_err(unavailable)?;
                let mut expired = Vec::new();
                for row in rows {
                    let record = instance_record(row)?;
                    if record.expires_at_us.is_some_and(|expiry| expiry <= now_us) {
                        expired.push(record);
                    }
                }
                Ok(expired)
            })
        })
        .await
    }

    async fn list_cleanup_pending(&self) -> Result<Vec<SandboxInstanceRecord>, SandboxStoreError> {
        self.run(move |tx| {
            Box::pin(async move {
                let rows = SandboxInstance::filter_by_desired_state("cleanup_pending")
                    .exec(tx)
                    .await
                    .map_err(unavailable)?;
                rows.into_iter().map(instance_record).collect()
            })
        })
        .await
    }



    async fn list_all_non_terminal(
        &self,
    ) -> Result<Vec<SandboxInstanceRecord>, SandboxStoreError> {
        self.run(move |tx| {
            Box::pin(async move {
                // 重启恢复需要全量扫描——按 desired_state 逐一列出非终态。
                let mut all = Vec::new();
                for state in ["provisioning", "initializing", "ready", "terminating", "cleanup_pending"] {
                    let rows = SandboxInstance::filter_by_desired_state(state)
                        .exec(tx)
                        .await
                        .map_err(unavailable)?;
                    all.extend(rows.into_iter().map(instance_record).collect::<Result<Vec<_>, _>>()?);
                }
                Ok(all)
            })
        })
        .await
    }

    // ---------- Volumes（M5） ----------

    async fn insert_volume(
        &self,
        record: &SandboxVolumeRecord,
    ) -> Result<SandboxVolumeRecord, SandboxStoreError> {
        let record = record.clone();
        self.run(move |tx| {
            let record = record.clone();
            Box::pin(async move {
                let count = toasty::sql::statement(
                    "INSERT INTO sandbox_volumes(volume_id,tenant_id,key_id,name,pvc_name,storage_class,size_gib,created_at_us,updated_at_us,deleted_at_us) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$8,NULL) ON CONFLICT(volume_id) DO NOTHING",
                )
                .bind(&record.volume_id)
                .bind(&record.tenant_id)
                .bind(&record.key_id)
                .bind(&record.name)
                .bind(&record.pvc_name)
                .bind(&record.storage_class)
                .bind(record.size_gib as i64)
                .bind(record.created_at_us)
                .exec(tx)
                .await
                .map_err(unavailable)?;
                let _ = count;
                let row = SandboxVolume::filter_by_volume_id(&record.volume_id)
                    .first()
                    .exec(tx)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(|| SandboxStoreError::Unavailable("volume insert row vanished".into()))?;
                volume_record(row)
            })
        })
        .await
    }

    async fn get_volume(
        &self,
        volume_id: &str,
    ) -> Result<Option<SandboxVolumeRecord>, SandboxStoreError> {
        let volume_id = volume_id.to_string();
        self.run(move |tx| {
            Box::pin(async move {
                let row = SandboxVolume::filter_by_volume_id(&volume_id)
                    .first()
                    .exec(tx)
                    .await
                    .map_err(unavailable)?;
                row.map(volume_record).transpose()
            })
        })
        .await
    }

    async fn find_volume_by_name(
        &self,
        tenant_id: &str,
        name: &str,
    ) -> Result<Option<SandboxVolumeRecord>, SandboxStoreError> {
        let tenant_id = tenant_id.to_string();
        let name = name.to_string();
        self.run(move |tx| {
            let tenant_id = tenant_id.clone();
            Box::pin(async move {
                let rows = SandboxVolume::filter_by_tenant_id(&tenant_id)
                    .exec(tx)
                    .await
                    .map_err(unavailable)?;
                rows.into_iter()
                    .filter(|row| row.name == name && row.deleted_at_us.is_none())
                    .next()
                    .map(volume_record)
                    .transpose()
            })
        })
        .await
    }

    async fn list_volumes(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<SandboxVolumeRecord>, SandboxStoreError> {
        let tenant_id = tenant_id.to_string();
        self.run(move |tx| {
            let tenant_id = tenant_id.clone();
            Box::pin(async move {
                let rows = SandboxVolume::filter_by_tenant_id(&tenant_id)
                    .exec(tx)
                    .await
                    .map_err(unavailable)?;
                let mut records: Vec<SandboxVolumeRecord> = rows
                    .into_iter()
                    .filter(|row| row.deleted_at_us.is_none())
                    .map(volume_record)
                    .collect::<Result<Vec<_>, _>>()?;
                records.sort_by(|a, b| b.created_at_us.cmp(&a.created_at_us));
                Ok(records)
            })
        })
        .await
    }

    async fn mark_volume_deleted(
        &self,
        volume_id: &str,
    ) -> Result<(), SandboxStoreError> {
        let volume_id = volume_id.to_string();
        self.run(move |tx| {
            Box::pin(async move {
                toasty::sql::statement(
                    "UPDATE sandbox_volumes SET deleted_at_us=$1,updated_at_us=$1 WHERE volume_id=$2 AND deleted_at_us IS NULL",
                )
                .bind(now_us())
                .bind(&volume_id)
                .exec(tx)
                .await
                .map_err(unavailable)?;
                Ok(())
            })
        })
        .await
    }

}

fn volume_record(row: SandboxVolume) -> Result<SandboxVolumeRecord, SandboxStoreError> {
    debug_assert!(
        (1..=100).contains(&row.size_gib),
        "volume size_gib {} 超出预期范围 1-100",
        row.size_gib
    );
    Ok(SandboxVolumeRecord {
        volume_id: row.volume_id,
        tenant_id: row.tenant_id,
        key_id: row.key_id,
        name: row.name,
        pvc_name: row.pvc_name,
        storage_class: row.storage_class,
        size_gib: row.size_gib as i32,
        created_at_us: row.created_at_us,
        updated_at_us: row.updated_at_us,
        deleted_at_us: row.deleted_at_us,
    })
}

async fn read_back(
    tx: &mut dyn Executor,
    sandbox_id: &str,
) -> Result<SandboxInstanceRecord, SandboxStoreError> {
    let row = SandboxInstance::filter_by_sandbox_id(sandbox_id)
        .first()
        .exec(tx)
        .await
        .map_err(unavailable)?
        .ok_or_else(|| {
            SandboxStoreError::Unavailable("sandbox row vanished in transaction".into())
        })?;
    instance_record(row)
}

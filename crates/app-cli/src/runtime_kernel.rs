//! 运行态单一所有者内核（阶段二，specs/userapp-runtime-ownership §3）。
//!
//! app-cli `serve` 是唯一运行态所有者：start/restart/deploy/stop 全部经
//! [`RuntimeKernel::admit`] 受理——短锁内完成鉴权/重放/冲突/修订/恢复检查，
//! 持久化 Accepted 记录后才派发执行（复用 server 主循环这一既有串行
//! worker：制品走部署通道，源码/停止走编排通道）。操作记录与事件按
//! `<workspace>/.app-cli-state/` 稳定状态根持久化，进程重建后可查询、
//! 可按 operation_id + request_digest 幂等重放。
//!
//! 关键不变量（spec R05/R06/R07）：
//! - 受理先持久化；终态发布前持久化；未确认停写保持恢复保护；
//! - HTTP/SSE 观察者断开不取消执行（worker 独立持有）；
//! - 同 operation_id 同摘要返回原结果；异摘要 409；
//! - stop 是持久化意图屏障：受理即提升 revision，旧构建提交被拒。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use shared_types::{
    DesiredState, ERR_OPERATION_ID_CONFLICT, ERR_OPERATION_IN_PROGRESS, ERR_RECOVERY_REQUIRED,
    ERR_REVISION_MISMATCH, ERR_RUNTIME_INSTANCE_MISMATCH, ERR_WORKSPACE_MISMATCH,
    RUNTIME_CONTROL_PROTOCOL_VERSION, RuntimeEventRecord, RuntimeFailureDetail,
    RuntimeIdentityView, RuntimeOperationKind, RuntimeOperationRequest, RuntimeOperationState,
    RuntimeOperationView, RuntimeStatusView, runtime_request_digest,
    validate_runtime_operation_request,
};
use tokio::sync::Mutex;

/// 稳定状态根（workspace 内、有效运行目录 `.run` 之外——plan §3.1）。
pub(crate) const STATE_DIR_NAME: &str = ".app-cli-state";

/// 受理判定结果。
#[derive(Debug)]
pub(crate) enum AdmissionOutcome {
    /// 幂等重放：返回既有记录（不重复执行）。
    Replayed(RuntimeOperationView),
    /// 新受理（已持久化 Accepted，等待执行）。
    Accepted(RuntimeOperationView),
}

/// 受理拒绝（API 层映射 HTTP 409/400 信封）。
#[derive(Debug)]
pub(crate) struct AdmissionRejection {
    pub code: &'static str,
    pub message: String,
    /// 进行中操作 ID（ERR_OPERATION_IN_PROGRESS 携带）。
    pub active_operation_id: Option<String>,
}

/// 持久状态根布局（identity/desired/operations/events 单一根）。
pub(crate) struct RuntimeStore {
    root: PathBuf,
}

impl RuntimeStore {
    pub(crate) fn open(workspace: &Path) -> Result<Self> {
        let root = workspace.join(STATE_DIR_NAME);
        std::fs::create_dir_all(root.join("operations")).context("create runtime state dir")?;
        std::fs::create_dir_all(root.join("events")).context("create runtime events dir")?;
        Ok(Self { root })
    }

    fn identity_path(&self) -> PathBuf {
        self.root.join("identity.json")
    }
    fn desired_path(&self) -> PathBuf {
        self.root.join("desired.json")
    }
    fn operation_path(&self, operation_id: &str) -> PathBuf {
        self.root
            .join("operations")
            .join(format!("{operation_id}.json"))
    }
    fn events_path(&self, operation_id: &str) -> PathBuf {
        self.root
            .join("events")
            .join(format!("{operation_id}.jsonl"))
    }

    /// 读取/落盘身份（runtime_instance_id 每次进程启动新生成；
    /// deployment_generation_id 延续既有代次）。
    pub(crate) fn load_or_init_identity(
        &self,
        application_id: String,
        service_family: String,
        workspace_id: String,
        source_root: String,
        deployment_generation_id: String,
    ) -> Result<RuntimeIdentityView> {
        let runtime_instance_id = uuid::Uuid::new_v4().to_string();
        let identity = RuntimeIdentityView {
            application_id,
            service_family,
            workspace_id,
            source_root,
            runtime_instance_id,
            deployment_generation_id,
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: vec![
                "operations".into(),
                "events".into(),
                "source-profile".into(),
                "artifact-profile".into(),
            ],
        };
        // identity.json 不回读旧 runtime_instance（旧实例身份不得复用），仅覆盖。
        write_json(&self.identity_path(), &identity)?;
        Ok(identity)
    }

    pub(crate) fn load_desired(&self) -> Result<(DesiredState, u64)> {
        let value: Option<serde_json::Value> = read_json(&self.desired_path())?;
        match value {
            Some(value) => {
                let desired = serde_json::from_value(value["desired"].clone())
                    .context("decode desired state")?;
                let revision = value["revision"].as_u64().context("decode revision")?;
                Ok((desired, revision))
            }
            None => Ok((DesiredState::Running, 0)),
        }
    }

    /// 持久化 desired + revision（单文件覆盖写，先 fsync 再可见）。
    pub(crate) fn store_desired(&self, desired: DesiredState, revision: u64) -> Result<()> {
        let value = serde_json::json!({ "desired": desired, "revision": revision });
        write_json(&self.desired_path(), &value)
    }

    pub(crate) fn load_operation(&self, operation_id: &str) -> Result<Option<StoredOperation>> {
        let value: Option<serde_json::Value> = read_json(&self.operation_path(operation_id))?;
        match value {
            Some(value) => Ok(Some(
                serde_json::from_value(value).context("decode operation")?,
            )),
            None => Ok(None),
        }
    }

    pub(crate) fn store_operation(&self, operation: &StoredOperation) -> Result<()> {
        write_json(
            &self.operation_path(&operation.view.operation_id),
            operation,
        )
    }

    /// 追加事件（每操作单序列；先落盘再发布——spec §3.6）。
    pub(crate) fn append_event(&self, event: &RuntimeEventRecord) -> Result<()> {
        use std::io::Write as _;
        let path = self.events_path(&event.operation_id);
        let mut line = serde_json::to_string(event).context("encode event")?;
        line.push('\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open event log {}", path.display()))?;
        file.write_all(line.as_bytes())
            .and_then(|_| file.sync_data())
            .with_context(|| format!("append event {}", path.display()))?;
        Ok(())
    }

    /// 重放事件（after_seq 语义：返回 sequence > after_seq 的记录）。
    pub(crate) fn replay_events(
        &self,
        operation_id: &str,
        after_seq: u64,
    ) -> Result<Vec<RuntimeEventRecord>> {
        let path = self.events_path(operation_id);
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("read event log {}", path.display()))?;
        let mut events = Vec::new();
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            let event: RuntimeEventRecord =
                serde_json::from_str(line).context("decode event record")?;
            if event.sequence > after_seq {
                events.push(event);
            }
        }
        events.sort_by_key(|event| event.sequence);
        Ok(events)
    }

    /// 启动恢复：最后一次非终态操作置 RecoveryRequired（不猜结果，写操作
    /// 被拒直至该操作显式恢复/重放完成）。
    pub(crate) fn recover_unfinished_operations(&self) -> Result<Vec<String>> {
        let mut recovered = Vec::new();
        let dir = self.root.join("operations");
        let entries = std::fs::read_dir(&dir).context("scan operations dir")?;
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(mut operation) =
                serde_json::from_str::<StoredOperation>(content.trim_end_matches('\n'))
            else {
                // 损坏记录 fail closed：保留文件并记录，不猜状态。
                tracing::error!(
                    "runtime state: undecodable operation record kept untouched: {}",
                    path.display()
                );
                continue;
            };
            if !operation.view.state.is_terminal() {
                operation.view.state = RuntimeOperationState::RecoveryRequired;
                operation.view.error_code = Some(ERR_RECOVERY_REQUIRED.into());
                operation.view.error_message =
                    Some("process restarted before the operation reached a terminal state".into());
                let id = operation.view.operation_id.clone();
                self.store_operation(&operation)?;
                recovered.push(id);
            }
        }
        Ok(recovered)
    }
}

/// 受理快照 + 对外视图（持久化单位）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredOperation {
    pub view: RuntimeOperationView,
    /// 受理时的规范化请求（重放摘要比对的原始输入）。
    pub request: RuntimeOperationRequest,
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create state parent dir")?;
    }
    let temp = path.with_extension("tmp");
    let content = serde_json::to_vec_pretty(value).context("encode state record")?;
    std::fs::write(&temp, &content)
        .and_then(|_| {
            let file = std::fs::File::open(&temp)?;
            file.sync_all()
        })
        .with_context(|| format!("persist state {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("publish state {}", path.display()))?;
    Ok(())
}

fn read_json(path: &Path) -> Result<Option<serde_json::Value>> {
    if !path.is_file() {
        return Ok(None);
    }
    let content =
        std::fs::read_to_string(path).with_context(|| format!("read state {}", path.display()))?;
    Ok(Some(
        serde_json::from_str(&content).context("decode state record")?,
    ))
}

/// 运行操作内核：受理/查询/事件/取消的协调面（执行仍归 server 主循环）。
pub(crate) struct RuntimeKernel {
    store: RuntimeStore,
    identity: RuntimeIdentityView,
    /// 受理短锁（不跨执行 await；执行互斥由 active_operation 单槽承担）。
    admission: Mutex<AdmissionState>,
    /// 执行派发回调（server 主循环注入；制品→部署通道，源码/停止→编排通道）。
    dispatch: Box<dyn Fn(DispatchAction) + Send + Sync>,
}

/// 执行派发动作（server 主循环解释；内核不直接触碰业务运行态）。
#[derive(Debug, Clone)]
pub(crate) enum DispatchAction {
    /// 制品部署（复用既有部署准备/激活/编排全链）。
    DeployArtifact {
        operation_id: String,
        url: String,
        sha256: Option<String>,
    },
    /// 源码编排（workspace 当前内容 + release lock；start/restart source）。
    OrchestrateSource { operation_id: String },
    /// 停止业务服务（保持管理面可用）。
    StopBusiness { operation_id: String },
}

#[derive(Default)]
struct AdmissionState {
    /// 当前进行中的操作（None = 空闲；单槽即单 worker 串行）。
    active_operation_id: Option<String>,
    /// 恢复保护中（上次结果未知）。
    recovery_protection: bool,
}

impl RuntimeKernel {
    pub(crate) fn new(
        store: RuntimeStore,
        identity: RuntimeIdentityView,
        dispatch: Box<dyn Fn(DispatchAction) + Send + Sync>,
    ) -> Self {
        Self {
            store,
            identity,
            admission: Mutex::new(AdmissionState::default()),
            dispatch,
        }
    }

    pub(crate) fn identity(&self) -> &RuntimeIdentityView {
        &self.identity
    }

    pub(crate) fn store(&self) -> &RuntimeStore {
        &self.store
    }

    /// 启动恢复入口（serve 主流程在 quiescence/ownership 之后调用）。
    pub(crate) async fn recover(&self) -> Result<Vec<String>> {
        let recovered = self.store.recover_unfinished_operations()?;
        let mut guard = self.admission.lock().await;
        if recovered.is_empty() {
            guard.recovery_protection = false;
        } else {
            // 恢复保护：有非终态操作被标记 RecoveryRequired 时拒绝新写，
            // 直至对应操作经查询/重放确认收束（spec §3.3 崩溃注入语义）。
            guard.recovery_protection = true;
        }
        Ok(recovered)
    }

    pub(crate) async fn status(&self) -> Result<RuntimeStatusView> {
        let (_desired, revision) = self.store.load_desired()?;
        let guard = self.admission.lock().await;
        Ok(RuntimeStatusView {
            desired: _desired,
            observed: shared_types::ObservedHealth::Unknown,
            active_target: None,
            revision,
            active_operation_id: guard.active_operation_id.clone(),
            recovery_protection: guard.recovery_protection,
            runtime_instance_id: self.identity.runtime_instance_id.clone(),
        })
    }

    /// 受理（spec §3.3）：短锁内完成全部检查与持久化，执行派发在锁外。
    pub(crate) async fn admit(
        &self,
        request: RuntimeOperationRequest,
    ) -> std::result::Result<AdmissionOutcome, AdmissionRejection> {
        if let Err(error) = validate_runtime_operation_request(&request) {
            return Err(AdmissionRejection {
                code: "ERR_VALIDATION",
                message: error,
                active_operation_id: None,
            });
        }
        let digest = runtime_request_digest(&request).map_err(|error| AdmissionRejection {
            code: "ERR_VALIDATION",
            message: error,
            active_operation_id: None,
        })?;
        // 身份核验先于一切重放判断：旧实例请求不修改新实例（spec §3.1）。
        if request.expected_runtime_instance_id != self.identity.runtime_instance_id {
            return Err(AdmissionRejection {
                code: ERR_RUNTIME_INSTANCE_MISMATCH,
                message: format!(
                    "expected runtime instance {} does not match current {}",
                    request.expected_runtime_instance_id, self.identity.runtime_instance_id
                ),
                active_operation_id: None,
            });
        }
        if request.workspace_id != self.identity.workspace_id {
            return Err(AdmissionRejection {
                code: ERR_WORKSPACE_MISMATCH,
                message: format!(
                    "workspace {} does not match owner binding {}",
                    request.workspace_id, self.identity.workspace_id
                ),
                active_operation_id: None,
            });
        }
        let mut guard = self.admission.lock().await;
        // 幂等重放先于 busy 拒绝（spec：先查重放再拒绝 busy）。
        if let Some(existing) =
            self.store
                .load_operation(&request.operation_id)
                .map_err(|error| AdmissionRejection {
                    code: "ERR_BACKEND_ERROR",
                    message: format!("read operation history: {error:#}"),
                    active_operation_id: None,
                })?
        {
            if existing.view.request_digest != digest {
                return Err(AdmissionRejection {
                    code: ERR_OPERATION_ID_CONFLICT,
                    message: format!(
                        "operation {} was accepted with a different request",
                        request.operation_id
                    ),
                    active_operation_id: None,
                });
            }
            return Ok(AdmissionOutcome::Replayed(existing.view));
        }
        if guard.recovery_protection {
            return Err(AdmissionRejection {
                code: ERR_RECOVERY_REQUIRED,
                message: "a previous operation has an unconfirmed result; recovery is required"
                    .into(),
                active_operation_id: None,
            });
        }
        let (_desired, revision) =
            self.store
                .load_desired()
                .map_err(|error| AdmissionRejection {
                    code: "ERR_BACKEND_ERROR",
                    message: format!("read desired state: {error:#}"),
                    active_operation_id: None,
                })?;
        // Stop 屏障例外：active 期间仍可受理持久化停止意图（spec §3.3）。
        let is_stop = request.kind == RuntimeOperationKind::Stop;
        if !is_stop {
            if let Some(active) = guard.active_operation_id.clone() {
                return Err(AdmissionRejection {
                    code: ERR_OPERATION_IN_PROGRESS,
                    message: "another runtime operation is in progress".into(),
                    active_operation_id: Some(active),
                });
            }
            if request.expected_revision != revision {
                return Err(AdmissionRejection {
                    code: ERR_REVISION_MISMATCH,
                    message: format!(
                        "expected revision {} but current revision is {}",
                        request.expected_revision, revision
                    ),
                    active_operation_id: None,
                });
            }
        } else if request.expected_revision != revision {
            // stop 的 revision 校验同样执行（旧实例的 stop 不复活/不重复推进）。
            return Err(AdmissionRejection {
                code: ERR_REVISION_MISMATCH,
                message: format!(
                    "expected revision {} but current revision is {}",
                    request.expected_revision, revision
                ),
                active_operation_id: None,
            });
        }
        // 持久化受理（落盘失败不入执行队列——spec §3.3）。
        let stored = StoredOperation {
            view: RuntimeOperationView {
                operation_id: request.operation_id.clone(),
                kind: request.kind,
                state: RuntimeOperationState::Accepted,
                request_digest: digest,
                revision,
                runtime_instance_id: self.identity.runtime_instance_id.clone(),
                error_code: None,
                error_message: None,
                failure_detail: None,
            },
            request,
        };
        if let Err(error) = self.store.store_operation(&stored) {
            return Err(AdmissionRejection {
                code: "ERR_BACKEND_ERROR",
                message: format!("persist accepted operation: {error:#}"),
                active_operation_id: None,
            });
        }
        // stop 受理即持久化意图并推进 revision：旧构建稍后提交被拒（spec §3.3）。
        if is_stop {
            let next_revision = revision.checked_add(1).ok_or_else(|| AdmissionRejection {
                code: "ERR_BACKEND_ERROR",
                message: "revision overflow".into(),
                active_operation_id: None,
            })?;
            if let Err(error) = self
                .store
                .store_desired(DesiredState::Stopped, next_revision)
            {
                return Err(AdmissionRejection {
                    code: "ERR_BACKEND_ERROR",
                    message: format!("persist stop intent: {error:#}"),
                    active_operation_id: None,
                });
            }
        } else if let Err(error) = self.store.store_desired(DesiredState::Running, revision) {
            return Err(AdmissionRejection {
                code: "ERR_BACKEND_ERROR",
                message: format!("persist running intent: {error:#}"),
                active_operation_id: None,
            });
        }
        guard.active_operation_id = Some(stored.view.operation_id.clone());
        let action = match (&stored.request.kind, &stored.request.profile) {
            (
                RuntimeOperationKind::Deploy,
                shared_types::RunProfileInput::Artifact {
                    artifact: shared_types::ArtifactInput::Url { url, sha256 },
                },
            ) => DispatchAction::DeployArtifact {
                operation_id: stored.view.operation_id.clone(),
                url: url.clone(),
                sha256: sha256.clone(),
            },
            (RuntimeOperationKind::Stop, _) => DispatchAction::StopBusiness {
                operation_id: stored.view.operation_id.clone(),
            },
            // start/restart(source/artifact) 与 deploy(source) = 确保运行：
            // 编排 workspace 当前有效内容（deploy 需显式制品输入，source 不强制）
            _ => DispatchAction::OrchestrateSource {
                operation_id: stored.view.operation_id.clone(),
            },
        };
        let operation_id = stored.view.operation_id.clone();
        self.emit(
            &operation_id,
            1,
            "accepted",
            None,
            Some(stored.view.kind.as_str()),
        );
        // 执行派发（server 主循环持有唯一执行权；内核不并发起第二个 worker）。
        (self.dispatch)(action);
        Ok(AdmissionOutcome::Accepted(stored.view))
    }

    pub(crate) async fn get(&self, operation_id: &str) -> Result<Option<RuntimeOperationView>> {
        Ok(self
            .store
            .load_operation(operation_id)?
            .map(|stored| stored.view))
    }

    /// 终态发布（server 主循环在执行完成/失败后调用；先持久化再清 active 槽）。
    pub(crate) async fn finish(
        &self,
        operation_id: &str,
        state: RuntimeOperationState,
        error: Option<(String, String)>,
        failure_detail: Option<RuntimeFailureDetail>,
        next_sequence: u64,
    ) -> Result<()> {
        let mut stored = self
            .store
            .load_operation(operation_id)?
            .context("finish unknown operation")?;
        let terminal = state.is_terminal() || state == RuntimeOperationState::RecoveryRequired;
        anyhow::ensure!(terminal, "finish requires a terminal or recovery state");
        stored.view.state = state;
        if let Some((code, message)) = error {
            stored.view.error_code = Some(code);
            stored.view.error_message = Some(message);
        }
        stored.view.failure_detail = failure_detail;
        self.store.store_operation(&stored)?;
        let event_name = match stored.view.state {
            RuntimeOperationState::Succeeded => "Completed",
            _ => "Failed",
        };
        self.emit(
            operation_id,
            next_sequence,
            "terminal",
            None,
            Some(event_name),
        );
        let mut guard = self.admission.lock().await;
        if guard.active_operation_id.as_deref() == Some(operation_id) {
            guard.active_operation_id = None;
            if stored.view.state == RuntimeOperationState::RecoveryRequired {
                guard.recovery_protection = true;
            }
        }
        Ok(())
    }

    /// 取消请求（受理态返回；不伪称立即完成——spec §3.2 cancel 语义）。
    pub(crate) async fn request_cancel(&self, operation_id: &str) -> Result<bool> {
        let guard = self.admission.lock().await;
        if guard.active_operation_id.as_deref() == Some(operation_id) {
            // 执行侧取消由 server 主循环观察 desired/cancel 通道实现；
            // 此处仅确认“操作存在且进行中”，不篡改状态。
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn emit(
        &self,
        operation_id: &str,
        sequence: u64,
        stage: &str,
        service: Option<String>,
        event_name: Option<&str>,
    ) {
        let record = RuntimeEventRecord {
            operation_id: operation_id.to_string(),
            sequence,
            runtime_instance_id: self.identity.runtime_instance_id.clone(),
            stage: stage.to_string(),
            service,
            event_name: event_name.map(str::to_string),
            payload: None,
        };
        if let Err(error) = self.store.append_event(&record) {
            // 事件落盘失败不阻断受理/执行主链（操作记录是权威），
            // 但必须可见——静默丢事件违反 spec §3.6。
            tracing::error!("runtime event persist failed (op {operation_id}): {error:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::{
        ArtifactInput, ERR_OPERATION_ID_CONFLICT, ERR_OPERATION_IN_PROGRESS, ERR_RECOVERY_REQUIRED,
        ERR_REVISION_MISMATCH, ERR_RUNTIME_INSTANCE_MISMATCH, ERR_WORKSPACE_MISMATCH,
        RunProfileInput, RuntimeOperationKind, RuntimeOperationRequest,
    };

    fn temp_store() -> (tempfile::TempDir, RuntimeStore) {
        let dir = tempfile::tempdir().expect("dir");
        let store = RuntimeStore::open(dir.path()).expect("store");
        (dir, store)
    }

    fn identity() -> RuntimeIdentityView {
        RuntimeIdentityView {
            application_id: "app1".into(),
            service_family: "userapp-dev".into(),
            workspace_id: "ws-1".into(),
            source_root: "/workspace".into(),
            runtime_instance_id: "instance-1".into(),
            deployment_generation_id: "gen-1".into(),
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: vec![],
        }
    }

    fn kernel(dir: &Path) -> RuntimeKernel {
        let store = RuntimeStore::open(dir).expect("store");
        let identity = identity();
        RuntimeKernel::new(store, identity, Box::new(|_| {}))
    }

    fn request(kind: RuntimeOperationKind, operation_id: &str) -> RuntimeOperationRequest {
        RuntimeOperationRequest {
            operation_id: operation_id.into(),
            expected_runtime_instance_id: "instance-1".into(),
            expected_revision: 0,
            workspace_id: "ws-1".into(),
            kind,
            profile: RunProfileInput::Source {
                workspace_id: "ws-1".into(),
            },
            request_context: None,
        }
    }

    #[tokio::test]
    async fn same_id_same_digest_replays_without_double_admission() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        let first = kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("admit");
        assert!(matches!(first, AdmissionOutcome::Accepted(_)));
        // 未 finish 前，同 ID 同摘要 = 重放（不是 busy 拒绝）
        let replay = kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("replay");
        assert!(matches!(replay, AdmissionOutcome::Replayed(_)));
    }

    #[tokio::test]
    async fn same_id_different_digest_conflicts() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("admit");
        let mut other = request(RuntimeOperationKind::Stop, "op-1");
        other.kind = RuntimeOperationKind::Restart;
        let rejection = kernel.admit(other).await.expect_err("conflict");
        assert_eq!(rejection.code, ERR_OPERATION_ID_CONFLICT);
    }

    #[tokio::test]
    async fn active_operation_blocks_other_kinds_but_not_stop() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("admit start");
        // start/restart/deploy 在 active 期间被拒（带进行中 ID）
        let rejection = kernel
            .admit(request(RuntimeOperationKind::Deploy, "op-2"))
            .await
            .expect_err("busy");
        assert_eq!(rejection.code, ERR_OPERATION_IN_PROGRESS);
        assert_eq!(rejection.active_operation_id.as_deref(), Some("op-1"));
        // stop 可受理（意图屏障），且推进 revision
        kernel
            .admit(request(RuntimeOperationKind::Stop, "op-stop"))
            .await
            .expect("stop admitted during active");
        let (_desired, revision) = kernel.store.load_desired().expect("desired");
        assert_eq!(revision, 1);
        // 原操作与 stop 均收束后，stop 推进的 revision 使旧 revision 部署提交被拒
        kernel
            .finish("op-1", RuntimeOperationState::Cancelled, None, None, 2)
            .await
            .expect("finish op-1");
        kernel
            .finish("op-stop", RuntimeOperationState::Succeeded, None, None, 2)
            .await
            .expect("finish stop");
        let mut stale = request(RuntimeOperationKind::Deploy, "op-3");
        stale.profile = RunProfileInput::Artifact {
            artifact: ArtifactInput::Url {
                url: "http://x/app.zip".into(),
                sha256: None,
            },
        };
        let rejection = kernel.admit(stale).await.expect_err("stale revision");
        assert_eq!(rejection.code, ERR_REVISION_MISMATCH);
    }

    #[tokio::test]
    async fn stale_runtime_instance_is_rejected_before_replay() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("admit");
        let mut stale = request(RuntimeOperationKind::Start, "op-1");
        stale.expected_runtime_instance_id = "previous-instance".into();
        let rejection = kernel.admit(stale).await.expect_err("instance mismatch");
        assert_eq!(rejection.code, ERR_RUNTIME_INSTANCE_MISMATCH);
    }

    #[tokio::test]
    async fn finish_clears_active_and_recovery_state_sets_protection() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("admit");
        kernel
            .finish(
                "op-1",
                RuntimeOperationState::RecoveryRequired,
                Some((ERR_RECOVERY_REQUIRED.into(), "cleanup unconfirmed".into())),
                None,
                2,
            )
            .await
            .expect("finish");
        let status = kernel.status().await.expect("status");
        assert!(status.recovery_protection);
        assert_eq!(status.active_operation_id, None);
        // 恢复保护期新写被拒
        let rejection = kernel
            .admit(request(RuntimeOperationKind::Start, "op-2"))
            .await
            .expect_err("recovery gate");
        assert_eq!(rejection.code, ERR_RECOVERY_REQUIRED);
    }

    #[tokio::test]
    async fn events_replay_by_after_seq_cursor() {
        let (dir, _keep) = temp_store();
        let store = RuntimeStore::open(dir.path()).expect("store");
        for sequence in 1..=3 {
            store
                .append_event(&RuntimeEventRecord {
                    operation_id: "op-9".into(),
                    sequence,
                    runtime_instance_id: "instance-1".into(),
                    stage: "stage".into(),
                    service: None,
                    event_name: Some("service_starting".into()),
                    payload: None,
                })
                .expect("append");
        }
        let all = store.replay_events("op-9", 0).expect("replay");
        assert_eq!(all.len(), 3);
        let tail = store.replay_events("op-9", 2).expect("replay");
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].sequence, 3);
    }

    #[tokio::test]
    async fn restart_recovery_marks_unfinished_operations() {
        let (dir, _keep) = temp_store();
        {
            let store = RuntimeStore::open(dir.path()).expect("store");
            store
                .store_operation(&StoredOperation {
                    view: RuntimeOperationView {
                        operation_id: "op-pending".into(),
                        kind: RuntimeOperationKind::Deploy,
                        state: RuntimeOperationState::Starting,
                        request_digest: "d".repeat(64),
                        revision: 0,
                        runtime_instance_id: "instance-old".into(),
                        error_code: None,
                        error_message: None,
                        failure_detail: None,
                    },
                    request: request(RuntimeOperationKind::Deploy, "op-pending"),
                })
                .expect("store");
        }
        // 新进程打开同一状态根：未终态操作转 RecoveryRequired + 保护
        let kernel = kernel(dir.path());
        let recovered = kernel.recover().await.expect("recover");
        assert_eq!(recovered, vec!["op-pending".to_string()]);
        let view = kernel
            .get("op-pending")
            .await
            .expect("get")
            .expect("present");
        assert_eq!(view.state, RuntimeOperationState::RecoveryRequired);
        let rejection = kernel
            .admit(request(RuntimeOperationKind::Start, "op-new"))
            .await
            .expect_err("recovery protection");
        assert_eq!(rejection.code, ERR_RECOVERY_REQUIRED);
    }

    #[tokio::test]
    async fn persist_failure_rejects_without_dispatch() {
        let (dir, _keep) = temp_store();
        // 目标操作记录路径预置为目录 → 原子写 rename 失败（受理持久化故障注入）
        let root = dir.path().join(STATE_DIR_NAME);
        std::fs::create_dir_all(root.join("operations").join("op-1.json")).expect("block path");
        let kernel = kernel(dir.path());
        let rejection = kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect_err("persist failure");
        assert_eq!(rejection.code, "ERR_BACKEND_ERROR");
        let status = kernel.status().await.expect("status");
        assert_eq!(status.active_operation_id, None, "no execution queued");
    }
}

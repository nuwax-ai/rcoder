//! E2B 兼容沙箱的领域类型与存储契约。
//!
//! 行为契约（与 userapp lifecycle 同标准）：
//! - 变更方法的成功响应严格发生在事务提交之后；失败响应不暗示事务未提交。
//! - 存储错误不等价于缺席：解析失败、数据库不可用必须返回 `Err`。
//! - 数据库事务内不做 K8s/HTTP 工作；外部资源释放遵循"先持久化证据 → 条件释放"。
//! - 实现不得调用运行时；无可选能力——完整后端实现全部方法。
//!
//! M1 范围：单副本、无 operations 台账（M3 引入）；状态机字段的语义见
//! `SandboxInstanceState` 文档。Guaranteed QoS 是沙箱 Pod 的硬约束
//! （Burstable 下 envd 的 oom wrapper 会被 kubelet floor 拒绝，实测结论）。

use serde::{Deserialize, Serialize};

/// 沙箱实例的期望状态（业务状态机；`observed` 投影由运行时观察填充）。
///
/// 转移：`Provisioning → Initializing → Ready → Terminating → Deleted`；
/// 异常侧路：任何状态可进入 `CleanupPending`（外部资源删除结果未知/失败，
/// 等待对账）与 `Failed`（带 failure_reason 的终态）。
/// `Deleted`/`Failed` 为终态，行保留作为生命周期证据（无级联删除）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum SandboxInstanceState {
    Provisioning,
    Initializing,
    Ready,
    Terminating,
    CleanupPending,
    Deleted,
    Failed,
}

impl SandboxInstanceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Initializing => "initializing",
            Self::Ready => "ready",
            Self::Terminating => "terminating",
            Self::CleanupPending => "cleanup_pending",
            Self::Deleted => "deleted",
            Self::Failed => "failed",
        }
    }
    pub fn terminal(self) -> bool {
        matches!(self, Self::Deleted | Self::Failed)
    }
}

/// 模板修订的不可变规格（实例创建时逐字段快照到实例行，注册表后续编辑不影响存量）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxTemplateSpec {
    pub template_id: String,
    pub revision: i64,
    pub image: String,
    pub image_digest: Option<String>,
    pub envd_version: String,
    /// `None` = runc；`Some(name)` = RuntimeClass（如 kata-clh-runtime-rs）。
    pub runtime_class: Option<String>,
    pub cpu_millicores: i64,
    pub memory_mib: i64,
    pub default_user: String,
    pub default_workdir: String,
    pub default_env: Vec<(String, String)>,
    pub ttl_max_seconds: i64,
    pub warm_target: i64,
}

/// E2B 门面展示用的模板头信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxTemplateSummary {
    pub template_id: String,
    pub display_name: Option<String>,
    pub enabled: bool,
    pub current_revision: i64,
}

/// API key 记录（仅持有校验摘要；明文只在创建时返回一次）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxApiKeyRecord {
    pub key_id: String,
    pub tenant_id: String,
    pub public_prefix: String,
    /// sha256 hex。
    pub key_hash: String,
    pub scopes: String,
    pub expires_at_us: Option<i64>,
    pub revoked_at_us: Option<i64>,
}

/// 沙箱实例行（存储全量投影；`*_at_us` 为微秒 Unix 时间）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxInstanceRecord {
    pub sandbox_id: String,
    pub tenant_id: String,
    pub key_id: String,
    pub template_id: String,
    pub template_revision: i64,
    pub image: String,
    pub image_digest: Option<String>,
    pub envd_version: String,
    pub runtime_class: Option<String>,
    pub cpu_millicores: i64,
    pub memory_mib: i64,
    pub desired_state: SandboxInstanceState,
    /// 运行时观察投影（如 pod phase/ready）；无观察为 `None`。
    pub observed_state: Option<String>,
    /// 乐观并发版本：每次状态变更 +1，CAS 写入必须携带读取时的值。
    pub instance_revision: i64,
    pub namespace: String,
    pub pod_name: String,
    pub pod_uid: Option<String>,
    /// sha256 hex of envdAccessToken。
    pub token_hash: String,
    /// AES-256-GCM 密文（base64）；密钥由引擎配置注入，不在存储层。
    pub encrypted_token: Option<String>,
    pub token_generation: i64,
    pub env: Vec<(String, String)>,
    pub metadata: Option<String>,
    pub failure_reason: Option<String>,
    pub created_at_us: i64,
    pub updated_at_us: i64,
    pub ready_at_us: Option<i64>,
    pub expires_at_us: Option<i64>,
    pub hard_deadline_us: Option<i64>,
    pub deleted_at_us: Option<i64>,
}

/// 创建沙箱的已验证入参（门面 DTO → 域类型，只含 M1 支持的字段）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSandboxParams {
    pub template_id: String,
    /// 存活 TTL（秒）；`None` 用模板默认。续期必须显式调用 timeout 接口。
    pub timeout_seconds: Option<i64>,
    pub env: Vec<(String, String)>,
    /// SDK metadata 透传（JSON 字符串原样存储）。
    pub metadata: Option<String>,
    /// 卷挂载（M5）：E2B volume name → 容器内路径。
    pub volume_mounts: Vec<VolumeMountRequest>,
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxStoreError {
    #[error("sandbox store unavailable: {0}")]
    Unavailable(String),
    #[error("sandbox record corrupt: {0}")]
    Corrupt(String),
    #[error("sandbox operation conflict: {0}")]
    Conflict(String),
    #[error("sandbox invalid operation: {0}")]
    InvalidOperation(String),
}

/// E2B 兼容沙箱存储契约（PG 实现：rcoder-storage；启用组件 `Sandbox`）。
#[async_trait::async_trait]
pub trait SandboxStore: Send + Sync {
    /// 注册表读取：模板当前启用修订的完整规格。
    async fn get_template_spec(
        &self,
        template_id: &str,
    ) -> Result<Option<SandboxTemplateSpec>, SandboxStoreError>;
    async fn list_templates(&self) -> Result<Vec<SandboxTemplateSummary>, SandboxStoreError>;

    /// 幂等种子写入（配置驱动；存在即跳过，不覆盖）。
    async fn seed_api_key(&self, record: &SandboxApiKeyRecord) -> Result<(), SandboxStoreError>;
    async fn seed_template(
        &self,
        summary: &SandboxTemplateSummary,
        spec: &SandboxTemplateSpec,
    ) -> Result<(), SandboxStoreError>;
    async fn find_api_key_by_hash(
        &self,
        key_hash: &str,
    ) -> Result<Option<SandboxApiKeyRecord>, SandboxStoreError>;

    /// 创建意图持久化（Provisioning 行，CAS 语义由主键幂等承担）。
    async fn insert_instance(
        &self,
        record: &SandboxInstanceRecord,
    ) -> Result<SandboxInstanceRecord, SandboxStoreError>;
    async fn get_instance(
        &self,
        sandbox_id: &str,
    ) -> Result<Option<SandboxInstanceRecord>, SandboxStoreError>;
    /// 状态转移（CAS）：`instance_revision` 不匹配返回 `Conflict`。
    /// 通用转移：不改时间戳语义的字段由各专用方法负责。
    async fn transition_state(
        &self,
        sandbox_id: &str,
        expected_revision: i64,
        new_state: SandboxInstanceState,
        failure_reason: Option<&str>,
        observed_state: Option<&str>,
    ) -> Result<SandboxInstanceRecord, SandboxStoreError>;
    /// 进入 Ready：记录 pod_uid、ready/expires 时间并转移状态。
    async fn mark_ready(
        &self,
        sandbox_id: &str,
        expected_revision: i64,
        pod_uid: Option<&str>,
        ready_at_us: i64,
        expires_at_us: i64,
    ) -> Result<SandboxInstanceRecord, SandboxStoreError>;
    /// 续期（timeout 接口）：仅更新 expires_at_us（不越过 hard_deadline，由调用方校验）。
    async fn set_expires(
        &self,
        sandbox_id: &str,
        expected_revision: i64,
        expires_at_us: i64,
    ) -> Result<SandboxInstanceRecord, SandboxStoreError>;
    /// 终态化（Deleted/Failed）：写 deleted_at_us 与原因。
    async fn finish(
        &self,
        sandbox_id: &str,
        expected_revision: i64,
        state: SandboxInstanceState,
        failure_reason: Option<&str>,
    ) -> Result<SandboxInstanceRecord, SandboxStoreError>;
    /// 租户列表（游标 = (created_at_us, sandbox_id) 严格降序分页）。
    async fn list_instances(
        &self,
        tenant_id: &str,
        before: Option<(i64, &str)>,
        limit: usize,
    ) -> Result<Vec<SandboxInstanceRecord>, SandboxStoreError>;
    /// 回收器扫描：非终态且 expires_at_us <= now 的实例。
    async fn list_expired(&self, now_us: i64) -> Result<Vec<SandboxInstanceRecord>, SandboxStoreError>;
    /// 非终态且 CleanupPending 的待对账实例（M3 使用；M1 返回空亦可）。
    async fn list_cleanup_pending(&self) -> Result<Vec<SandboxInstanceRecord>, SandboxStoreError>;

    // ---------- Volumes（M5） ----------
    async fn insert_volume(
        &self,
        record: &SandboxVolumeRecord,
    ) -> Result<SandboxVolumeRecord, SandboxStoreError>;
    async fn get_volume(
        &self,
        volume_id: &str,
    ) -> Result<Option<SandboxVolumeRecord>, SandboxStoreError>;
    /// 按租户+名称查找（未删除的；创建沙箱挂载用）。
    async fn find_volume_by_name(
        &self,
        tenant_id: &str,
        name: &str,
    ) -> Result<Option<SandboxVolumeRecord>, SandboxStoreError>;
    async fn list_volumes(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<SandboxVolumeRecord>, SandboxStoreError>;
    /// 软删除（deleted_at_us 置值；物理 PVC 删除由服务层协调）。
    async fn mark_volume_deleted(
        &self,
        volume_id: &str,
    ) -> Result<(), SandboxStoreError>;
}

/// sha256 hex（token/api key 摘要的唯一哈希入口）。
pub fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let out = hasher.finalize();
    let mut hex = String::with_capacity(out.len() * 2);
    for b in out {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

// ---------- E2B Volumes（M5：持久卷） ----------

/// E2B Volume 记录（存储投影）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxVolumeRecord {
    pub volume_id: String,
    pub tenant_id: String,
    pub key_id: String,
    /// 用户可读名称（租户内唯一，未删除的）。
    pub name: String,
    /// K8s PVC 名称（e2b-vol-{volume_id}）。
    pub pvc_name: String,
    pub storage_class: String,
    pub size_gib: i32,
    pub created_at_us: i64,
    pub updated_at_us: i64,
    pub deleted_at_us: Option<i64>,
}

/// create sandbox 时的卷挂载请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeMountRequest {
    /// E2B volume 名称（租户内查找）。
    pub volume_name: String,
    /// 沙箱内挂载路径（如 /home/user/data）。
    pub mount_path: String,
}

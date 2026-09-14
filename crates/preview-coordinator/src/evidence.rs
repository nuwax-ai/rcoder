//! Unknown 实例的恢复证据（宿主 Pod 存在性核实）。
//!
//! K8s：rcoder 注入 kube 实现（列 `component=rcoder-main` Pod 比 UID）；
//! 单机/Compose：非本进程 host_id 即前代进程（进程死亡=容器死亡）。

/// 宿主存在性证据源。
#[async_trait::async_trait]
pub trait HostEvidence: Send + Sync {
    /// 原 Pod UID 是否仍存在于集群。K8s 语义=Pod 对象仍存活；单机语义恒 false
    /// （不存在其他宿主）。
    async fn host_pod_exists(&self, pod_uid: &str) -> bool;
}

/// 单实例实现（Compose/本地）：唯一宿主是本进程；任何他 host_id 的活跃行
/// 都属于已终止的前代进程——宿主"不存在"成立，恢复证据成立。
pub struct SingleInstanceEvidence;

#[async_trait::async_trait]
impl HostEvidence for SingleInstanceEvidence {
    async fn host_pod_exists(&self, _pod_uid: &str) -> bool {
        false
    }
}

/// 从 host_id（`{pod_uid}:{boot_id}`）取 pod_uid 段。
/// pod_uid 为 UUID/hostname（无冒号），首个冒号前即 UID。
pub fn pod_uid_of(host_id: &str) -> &str {
    host_id.split(':').next().unwrap_or(host_id)
}

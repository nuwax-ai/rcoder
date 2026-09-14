//! 本进程宿主身份构造（Downward API 注入的 POD_UID/POD_IP/POD_NAME + 进程代次）。
use shared_types::PreviewHostIdentity;
use std::sync::LazyLock;

/// rcoder 进程启动代次：同一 Pod 内进程重启即变化（boot_id 变化是旧实例判停的
/// 直接证据——容器重启销毁 PID 命名空间）。
static BOOT_ID: LazyLock<String> = LazyLock::new(|| uuid::Uuid::new_v4().to_string());

/// POD_UID（Downward API `metadata.uid`）；缺失时回退 HOSTNAME（容器/Pod 名）。
static POD_UID: LazyLock<Option<String>> = LazyLock::new(|| {
    std::env::var("POD_UID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
});

/// POD_IP（Downward API `status.podIP`）。
static POD_IP: LazyLock<Option<String>> = LazyLock::new(|| env_nonempty("POD_IP"));

/// POD_NAME（Downward API `metadata.name`）。
static POD_NAME: LazyLock<Option<String>> = LazyLock::new(|| env_nonempty("POD_NAME"));

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

/// 本进程宿主身份。POD_UID 缺失（本地裸跑）时 host_id 退化为
/// `{hostname}:{boot_id}`——单实例语义仍成立（boot_id 区分进程重启）。
pub fn local_host_identity() -> PreviewHostIdentity {
    let pod_uid = POD_UID.clone().unwrap_or_else(|| "local".into());
    PreviewHostIdentity {
        host_id: format!("{pod_uid}:{}", *BOOT_ID),
        pod_name: POD_NAME.clone().or_else(|| POD_UID.clone()),
        pod_ip: POD_IP.clone(),
    }
}

/// 启动对账参数（pod_uid, boot_id）。
pub fn reboot_reconcile_args() -> (String, String) {
    let pod_uid = POD_UID.clone().unwrap_or_else(|| "local".into());
    (pod_uid, (*BOOT_ID).clone())
}

pub fn boot_id() -> &'static str {
    &BOOT_ID
}

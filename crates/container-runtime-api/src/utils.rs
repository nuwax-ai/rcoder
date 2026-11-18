//! Container runtime 工具函数（日志行解析；Quantity 解析已下沉到 shared_types）

// K8s Quantity 工具已下沉到 shared_types（共享，跨 crate 复用）
// 这里 re-export 保持 container_runtime_api::parse_memory_quantity 向后兼容
pub use shared_types::{parse_memory_quantity, validate_k8s_storage_size};

/// 拆分带时间戳的容器日志行。
///
/// `timestamps=true` 时 K8s `logs` 与 `docker logs` 行格式均为 `<RFC3339> <message>`
/// （首个空格分隔时间戳与内容），返回 `(Some(ts), msg)`；否则返回 `(None, 整行)`。
/// 两种 runtime 共用此解析，避免逻辑重复。
pub fn split_log_timestamp(line: &str, timestamps: bool) -> (Option<String>, String) {
    if timestamps && let Some(idx) = line.find(' ') {
        let (ts, rest) = line.split_at(idx);
        // rest 以 ' '（单字节 ASCII）开头，跳过 1 字节后必落在字符边界
        let msg = if rest.len() > 1 { &rest[1..] } else { "" };
        return (Some(ts.to_string()), msg.to_string());
    }
    (None, line.to_string())
}

/// 镜像引用的仓库段（`registry/ns/app:tag@digest` → `registry/ns/app`）。
/// 最后一个不在路径段内的冒号起点是 tag；digest 后缀忽略。daemon 内容
/// ID（`sha256:…`）无仓库段，返回 `sha256`，永不与平台仓库相等。
///
/// 启动路径镜像对齐的归属守卫共用（K8s 与 Docker 同一判定）：仅当现有
/// 模板/容器镜像与平台当前引用同仓库时才滚动到新 tag；外部或显式钉住
/// 的镜像保留原样（spec sandbox-image-version-drift R1）。
pub fn image_repository(reference: &str) -> &str {
    let without_digest = reference.split('@').next().unwrap_or(reference);
    match without_digest.rsplit_once(':') {
        Some((repository, tag)) if !tag.is_empty() && !tag.contains('/') => repository,
        _ => without_digest,
    }
}

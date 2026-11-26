//! 挂载模板里的变量替换，以及"在哪个路径上建目录"。

use std::collections::HashMap;

use shared_types::ServiceMountConfig;

/// 把模板里的 `{变量}` 换成 `values` 里的值
pub(super) fn substitute(template: &str, values: &HashMap<String, String>) -> String {
    values
        .iter()
        .fold(template.to_owned(), |rendered, (key, value)| {
            rendered.replace(&format!("{{{key}}}"), value)
        })
}

/// `host_path` 模板里 `{resolved_path}` 之后的部分，变量已替换；模板不是以 `{resolved_path}`
/// 开头时为空
pub(super) fn path_after_resolved_base(
    mount_config: &ServiceMountConfig,
    values: &HashMap<String, String>,
) -> String {
    substitute(
        mount_config
            .host_path
            .strip_prefix("{resolved_path}")
            .unwrap_or(""),
        values,
    )
}

/// RCoder 进程里用来建目录的路径，它对应宿主机上的目录 `host_path`。
///
/// `host_path` 必须是这个目录在宿主机上的**完整**路径，`suffix` 是它在 `{resolved_path}` 之后
/// 的部分（两者要指向同一个目录）。
///
/// 容器形态：用容器内的路径（compose bind 双向同步宿主机，rcoder 进程经挂载卷可写）。
/// deploy-host：rcoder 进程即宿主机进程，直接在 bind 源上创建——容器常量路径在宿主机常为
/// 只读根。
pub(super) fn directory_visible_to_this_process(
    host_path: &str,
    resolve_from: &str,
    suffix: &str,
) -> String {
    #[cfg(feature = "deploy-host")]
    if shared_types::is_deploy_host() {
        return host_path.to_owned();
    }
    #[cfg(not(feature = "deploy-host"))]
    let _ = host_path;
    format!("{resolve_from}{suffix}")
}

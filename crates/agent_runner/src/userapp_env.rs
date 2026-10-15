//! Userapp builder 容器内的运行时路径解析（与 file-server 同源）。
//!
//! file-server 全部 userapp 域接口定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`
//! （env 驱动，见 file-server config/load.rs 的 `path!` 读取）；builder 容器由
//! 平台注入该 env（K8s k8s_agent_env.rs / Docker agent_container_starter，
//! 值 = `shared_types::paths::USERAPP_DEV_HOME`）。chat 工作目录与终端 cwd
//! 消费同一来源，三方同根——缺省回落 shared_types 单一事实源，env 缺失时
//! 语义仍落在压平契约（paths.rs 60-90 行）上。

/// Userapp builder 容器内 workspace 根目录。
///
/// workspace = `{返回值}/{app_id}`（勿再加 `userapp-workspace` 段——
/// `paths::USERAPP_WORKSPACE_ROOT` 是沙箱 ComputerAgentRunner 视角的共享卷
/// 挂载点，在 builder 容器内不存在）。
pub(crate) fn userapp_workspace_dir() -> String {
    std::env::var("USERAPP_WORKSPACE_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| shared_types::paths::USERAPP_DEV_HOME.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_to_flatten_contract_default() {
        // 测试环境不设 USERAPP_WORKSPACE_DIR → 回落压平契约缺省值
        // （CI 无该 env；若本地意外设置了，跳过断言避免 flaky）
        if std::env::var("USERAPP_WORKSPACE_DIR").is_ok() {
            return;
        }
        assert_eq!(userapp_workspace_dir(), "/home/user");
    }
}

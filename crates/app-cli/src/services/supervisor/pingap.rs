use super::*;
use crate::proxy::compiler::CompileOutcome;

// ── pingap 启动 ─────────────────────────────────────────────────────────────────

/// 直跑形态（无 owner 会话作用域）：pingap 进**业务会话树**（R01 受管
/// 停止/收束链——run() 返回时整组收束，无孤儿）。serve 形态的常驻入口
/// 见 [`super::resident`]。编译/发布 active 由调用方完成。
pub(super) async fn spawn_pingap_into_session_tree(
    outcome: &CompileOutcome,
    pingap_bin: &Path,
    active_config: &Path,
    children: &mut ManagedChildren,
) -> Result<()> {
    // admin 仅启用为 loopback 只读确认通道；TOML 仍是唯一配置权威；永不通过 admin 写配置。
    let endpoint = admin_probe::ensure_admin_endpoint()?;

    // R01：pingap 受管进程树 spawn（与业务服务同一停止/收束链）。
    let mut cmd = Command::new(pingap_bin);
    cmd.arg("-c")
        .arg(active_config)
        .arg("--autoreload")
        // pingap 的 env override 规则：`get_from_env(key)` 读 `PINGAP_{key}` 全大写
        //（pingap src/main.rs parse_arguments 的闭包），故必须用 PINGAP_ADMIN_* 而非 admin_*。
        // 凭证经 env 注入（不进命令行避免 ps 泄露、不落盘不进日志），admin 仅 loopback 只读。
        .env("PINGAP_ADMIN_ADDR", &endpoint.addr)
        .env("PINGAP_ADMIN_USER", &endpoint.user)
        .env("PINGAP_ADMIN_PASSWORD", &endpoint.password);
    let child = process_utils::guardian::spawn_owned_with_output(cmd, None, false)
        .await
        .context("spawn pingap")?;
    info!(
        "🚀 start pingap on :{} (pid={})",
        PINGAP_PORT,
        child.id().unwrap_or(0)
    );
    children.push(("pingap".into(), child));

    let result =
        admin_probe::wait_for_publication(endpoint, outcome, admin_probe::CONFIRM_BUDGET).await;
    match result {
        Ok(confirmed) => {
            anyhow::ensure!(
                children.last().and_then(|(_, child)| child.id()) == Some(confirmed.process_id),
                "proxy application belongs to another process"
            );
            crate::proxy::compiler::record_confirmed_publication(outcome, &confirmed)?;
            info!("pingap initial complete graph and listener HTTP confirmed");
        }
        Err(error) => {
            shutdown_all(std::mem::take(children), 5).await?;
            return Err(error).context("confirm initial Pingap complete application");
        }
    }
    Ok(())
}

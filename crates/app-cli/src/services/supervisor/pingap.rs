use super::*;

// ── pingap 启动 ─────────────────────────────────────────────────────────────────

/// 编译用户权威配置到只读运行目录，`pingap -t` 成功后再启动，
/// 并经 loopback admin 只读通道确认初始配置实际生效。
pub(super) async fn start_pingap(
    ws_root: &Path,
    log_root: &Path,
    pingap_bin: &Path,
    release: &workspace_manifest::ReleaseLock,
    children: &mut ManagedChildren,
    dev_profile: bool,
) -> Result<()> {
    // N02：运行目录默认不假设容器 /run（原生 Windows/macOS 不可写）——
    // env 显式优先（平台注入容器布局），缺省挂 log_dir 子目录（用户可写、
    // 稳定、非系统临时目录；配置每次启动重生成，随日志卷持久无害）。
    let runtime_root = crate::proxy::compiler::runtime_root(log_root);
    let outcome =
        compile_and_validate(ws_root, &runtime_root, pingap_bin, release, dev_profile).await?;
    info!(
        "📝 effective pingap config → {}",
        outcome.config_path.display()
    );

    // admin 仅启用为 loopback 只读确认通道；TOML 仍是唯一配置权威；永不通过 admin 写配置。
    // 凭证每次启动随机生成，经 env 注入（不进命令行，避免 ps 泄露；不落盘不进日志）。
    let admin_addr = format!("127.0.0.1:{}", admin_probe::admin_port());
    let endpoint = admin_probe::register_admin_endpoint(
        admin_addr,
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    );

    // R01：pingap 受管进程树 spawn（与业务服务同一停止/收束链）。
    let mut cmd = Command::new(pingap_bin);
    cmd.arg("-c")
        .arg(&outcome.config_path)
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

    // 初始确认：pingap 必须真正加载当前配置（config_hash 匹配），否则视为启动失败，
    // 返回 Err 触发 supervisor 整组重启语义；失败前优雅停止已启动的子进程避免残留。
    //
    // 本地开发逃生开关 APP_CLI_SKIP_PINGAP_CONFIRM：跳过 admin probe 确认（pingap 仍以
    // --autoreload 启动；配置正确性已由 `pingap -t` 语法校验 + 实际 curl 验证兜底）。生产不设。
    if std::env::var_os("APP_CLI_SKIP_PINGAP_CONFIRM").is_some() {
        warn!(
            "⏭  APP_CLI_SKIP_PINGAP_CONFIRM set; skipping initial pingap config confirmation (dev only)"
        );
    } else if let Err(error) = admin_probe::wait_for_config_hash(
        endpoint,
        &outcome.expected_hash,
        admin_probe::CONFIRM_BUDGET,
    )
    .await
    {
        error!("❌ pingap initial config confirmation failed: {error:#}");
        shutdown_all(std::mem::take(children), 5).await?;
        return Err(error).context("confirm initial Pingap config via loopback admin probe");
    } else {
        // 已确认生效：业务就绪观察以此核对 admin 实际 hash。
        crate::proxy::compiler::record_expected_hash(&outcome.expected_hash);
        info!("✅ pingap initial config confirmed (config_hash matched)");
    }
    Ok(())
}

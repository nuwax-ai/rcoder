//! 本地开发工具：`app-cli --gen-lock <workspace>`。
//!
//! 复用 workspace-manifest 的纯函数（发现 → 校验 → 锁定）+ app-cli proxy 的纯编译，
//! 在本地秒级生成 `release.lock.toml` 并预览 Pingap 生效配置 —— 无需 pingap 二进制 / PG / 镜像。
//! 供 UserApp 模板与 manifest 设计的快速迭代验证："改 toml → 重跑 → 立刻看端口/拓扑/路由/Pingap 配置"。

use std::path::Path;

use anyhow::{Context, Result};
use workspace_manifest::{ReleaseMetadata, build_release_lock, discover_projects, parse_workspace};

use crate::proxy::compiler::compile_effective_config;

/// 为 `<workspace>` 生成 `release.lock.toml`，并打印诊断信息：
/// 发现的服务、端口分配 + 拓扑序、以及编译出的 Pingap 生效配置 TOML。
///
/// `ReleaseMetadata` 的 pingap 版本/镜像 digest 用本地占位值（仅影响日志追溯，不影响校验）；
/// `minimum_app_cli_version` 取当前 app-cli 版本，确保紧接着能用本二进制 `run` 起来。
pub async fn gen_lock(workspace: &Path) -> Result<()> {
    let ws_path = workspace.join("workspace.manifest.toml");
    let ws_content =
        std::fs::read_to_string(&ws_path).with_context(|| format!("read {}", ws_path.display()))?;
    let ws_manifest = parse_workspace(&ws_content).context("parse workspace.manifest.toml")?;

    let projects = discover_projects(workspace).context("discover projects")?;

    println!("📋 发现 {} 个 enabled 服务:", projects.len());
    for project in &projects {
        println!(
            "   • {:<24} dir={:<22} type={:?} kind={:?}",
            project.service_id(),
            project.dir,
            project.manifest.project.r#type,
            project.manifest.project.kind,
        );
    }

    let release_id = uuid::Uuid::new_v4().simple().to_string();
    let lock = build_release_lock(
        &ws_manifest,
        &projects,
        ReleaseMetadata {
            release_id: &release_id,
            pingap_version: "0.13.8",
            pingap_commit: "07d1cce",
            minimum_app_cli_version: env!("CARGO_PKG_VERSION"),
            runtime_image_digest: "local-dev",
        },
    )
    .context("build release lock（manifest 校验未通过）")?;

    println!("\n🔌 端口分配 + 启动拓扑序（release.lock.services 顺序 = migrate/启动顺序）:");
    for service in &lock.services {
        let proxy = service
            .proxy
            .as_ref()
            .map(|proxy| format!("{} (strip_prefix={})", proxy.path, proxy.strip_prefix))
            .unwrap_or_else(|| "— (无 [proxy]，不对外暴露)".into());
        println!(
            "   • {:<24} port={}  proxy={}",
            service.service_id, service.port, proxy
        );
    }

    // 编译 Pingap 生效配置（纯函数：不调 pingap 二进制、不落盘）。
    let (pingap_toml, hash) = compile_effective_config(workspace, &lock)
        .await
        .context("compile effective Pingap config")?;

    println!("\n🛡  Pingap 生效配置 (expected hash = {hash}):");
    println!("──── pingap.toml ────");
    println!("{pingap_toml}──── end ────");

    let lock_path = workspace.join("release.lock.toml");
    let lock_toml = toml::to_string_pretty(&lock).context("serialize release lock")?;
    std::fs::write(&lock_path, &lock_toml)
        .with_context(|| format!("write {}", lock_path.display()))?;
    println!("\n✅ release.lock.toml 已写入: {}", lock_path.display());
    println!(
        "   现在可以: APP_CLI_WORKSPACE={} APP_CLI_PINGAP_BIN=<pingap> app-cli",
        workspace.display()
    );
    Ok(())
}

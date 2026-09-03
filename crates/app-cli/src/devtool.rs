//! 本地开发工具：`app-cli --gen-lock <workspace>`。
//!
//! 复用 workspace-manifest 的纯函数（发现 → 校验 → 锁定）+ app-cli proxy 的纯编译，
//! 在本地秒级生成 `release.lock.toml` 并预览 Pingap 生效配置 —— 无需 pingap 二进制 / PG / 镜像。
//! 供 Userapp 模板与 manifest 设计的快速迭代验证："改 toml → 重跑 → 立刻看端口/拓扑/路由/Pingap 配置"。

use std::path::Path;

use anyhow::{Context, Result};
use workspace_manifest::{
    ReleaseMetadata, build_release_lock, discover_projects_lenient, parse_workspace,
};

use crate::proxy::compiler::compile_effective_config;

/// 本地无环境变量时的 pingap 身份回退值（升级 pingap 时与 Cargo.toml 的
/// pingap-config git rev、build-agent-docker 16-app-runtime.mk 的 PINGAP_COMMIT 一起改）。
const DEFAULT_PINGAP_VERSION: &str = "0.13.9";
const DEFAULT_PINGAP_COMMIT: &str = "f7f9eddb029a5b07438bead2e0fd3df763086567";

/// pingap 身份优先读 `RCODER_PINGAP_VERSION`/`RCODER_PINGAP_COMMIT`（与 file-server
/// 真实发布链路同名；容器内由镜像 ENV 注入，见 16-app-runtime.mk 单一版本源），
/// 使 devtool 在容器内外都报告所在运行时的真实 pingap 版本；本地未设置才回退常量。
fn pingap_identity() -> (String, String) {
    let read = |name: &str, fallback: &str| {
        std::env::var(name)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| fallback.to_string())
    };
    (
        read("RCODER_PINGAP_VERSION", DEFAULT_PINGAP_VERSION),
        read("RCODER_PINGAP_COMMIT", DEFAULT_PINGAP_COMMIT),
    )
}

/// 为 `<workspace>` 生成 `release.lock.toml`，并打印诊断信息：
/// 发现的服务、端口分配 + 拓扑序、以及编译出的 Pingap 生效配置 TOML。
///
/// `ReleaseMetadata` 的 pingap 版本/commit 来自环境变量（缺省回退上方常量，仅影响日志追溯）；
/// 镜像 digest 用本地占位值；`minimum_app_cli_version` 取当前 app-cli 版本，
/// 确保紧接着能用本二进制 `run` 起来。
pub async fn gen_lock(workspace: &Path) -> Result<()> {
    let ws_path = workspace.join("workspace.manifest.toml");
    let ws_content =
        std::fs::read_to_string(&ws_path).with_context(|| format!("read {}", ws_path.display()))?;
    let ws_manifest = parse_workspace(&ws_content).context("parse workspace.manifest.toml")?;

    // 宽松发现：单模块 TOML/校验错误不中断扫描，全部问题一次呈现——
    // 用户/agent 拿到完整清单可一轮修完，而不是"修一个 → 重跑 → 下一个错"。
    let (projects, issues) = discover_projects_lenient(workspace).context("discover projects")?;
    if !issues.is_empty() {
        println!("❌ manifest 校验发现 {} 个问题:", issues.len());
        for (index, issue) in issues.iter().enumerate() {
            println!("  {}. {}", index + 1, issue);
        }
        anyhow::bail!(
            "manifest validation failed with {} issue(s); fix the files above and re-run \
             (note: any existing release.lock.toml was NOT updated — do not run against a stale lock)",
            issues.len()
        );
    }

    println!("📋 发现 {} 个 enabled 服务:", projects.len());
    for project in &projects {
        // dev 段标注：dev 阶段命令与生产不同时可见（排障：dev 链路为何走了不同命令）
        let dev_tags = [
            project.manifest.devbuild.is_some().then_some("devbuild"),
            project.manifest.devrun.is_some().then_some("devrun"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(",");
        let dev_note = if dev_tags.is_empty() {
            String::new()
        } else {
            format!(" dev=[{dev_tags}]")
        };
        println!(
            "   • {:<24} dir={:<22} type={:?} kind={:?}{dev_note}",
            project.service_id(),
            project.dir,
            project.manifest.project.r#type,
            project.manifest.project.kind,
        );
    }

    let release_id = uuid::Uuid::new_v4().simple().to_string();
    let (pingap_version, pingap_commit) = pingap_identity();
    let lock = build_release_lock(
        &ws_manifest,
        &projects,
        ReleaseMetadata {
            release_id: &release_id,
            pingap_version: &pingap_version,
            pingap_commit: &pingap_commit,
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

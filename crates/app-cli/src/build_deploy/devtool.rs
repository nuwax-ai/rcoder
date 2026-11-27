//! 本地开发工具：`app-cli gen-lock --workspace <workspace>`。
//!
//! 复用 workspace-manifest 的纯函数（发现 → 校验 → 锁定）+ app-cli proxy 的纯编译，
//! 在本地秒级生成 `release.lock.toml` 并预览 Pingap 生效配置 —— 无需 pingap 二进制 / PG / 镜像。
//! 供 Userapp 模板与 manifest 设计的快速迭代验证："改 toml → 重跑 → 立刻看端口/拓扑/路由/Pingap 配置"。

use std::path::Path;

use anyhow::{Context, Result};
use workspace_manifest::ReleaseMetadata;

use super::inspection::{InspectionPurpose, inspect};

/// 本地无环境变量时的 pingap 身份回退值（升级 pingap 时与 Cargo.toml 的
/// pingap-config 版本、tools/build/pingap-assets.json 的 releases 条目一起改；
/// 三方由 tools/build/pingap_identity.py 门禁校验一致）。
/// commit 为 v0.16.1 tag 的官方提交（config crate 版本必须与镜像 pingap
/// 二进制联动，见 16-app-runtime.mk 单一版本源）。
pub(crate) const DEFAULT_PINGAP_VERSION: &str = "0.16.1";
pub(crate) const DEFAULT_PINGAP_COMMIT: &str = "695538b2745dcf9df812d2fa8f2e4ad8c7cbb1fb";

/// pingap 身份优先读 `RCODER_PINGAP_VERSION`/`RCODER_PINGAP_COMMIT`（与 file-server
/// 真实发布链路同名；容器内由镜像 ENV 注入，见 16-app-runtime.mk 单一版本源），
/// 使 devtool 在容器内外都报告所在运行时的真实 pingap 版本；本地未设置才回退常量。
pub(crate) fn pingap_identity() -> (String, String) {
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
pub async fn gen_lock(workspace: &Path, dev_profile: bool) -> Result<()> {
    let release_id = uuid::Uuid::new_v4().simple().to_string();
    let (pingap_version, pingap_commit) = pingap_identity();
    let inspected = inspect(
        workspace,
        dev_profile,
        ReleaseMetadata {
            release_id: &release_id,
            pingap_version: &pingap_version,
            pingap_commit: &pingap_commit,
            minimum_app_cli_version: env!("CARGO_PKG_VERSION"),
            runtime_image_digest: "local-dev",
        },
        InspectionPurpose::GenLock,
    )
    .await;
    if !inspected.report.valid {
        super::validate::write_text(&mut std::io::stdout().lock(), &inspected.report)?;
        anyhow::bail!(
            "configuration validation failed; any existing release.lock.toml was NOT updated — do not run against a stale lock"
        );
    }
    let projects = inspected.projects;
    let lock = inspected
        .lock
        .context("inspection succeeded without a release lock")?;
    let (pingap_toml, hash) = inspected
        .proxy
        .context("inspection succeeded without proxy configuration")?;

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

    println!("\n🔌 端口分配 + 启动拓扑序（release.lock.services 顺序 = migrate/启动顺序）:");
    for service in &lock.services {
        let proxy = service
            .proxy
            .as_ref()
            .map(|proxy| {
                let strip_prefix =
                    proxy.effective_strip_prefix(dev_profile && service.devrun.is_some());
                format!("{} (strip_prefix={strip_prefix})", proxy.path)
            })
            .unwrap_or_else(|| "— (无 [proxy]，不对外暴露)".into());
        println!(
            "   • {:<24} port={}  proxy={}",
            service.service_id, service.port, proxy
        );
    }

    let profile = if dev_profile { "dev" } else { "prod" };
    println!("\n🛡  Pingap 生效配置 (profile = {profile}, expected hash = {hash}):");
    println!("──── pingap.toml ────");
    println!("{pingap_toml}──── end ────");

    let lock_path = workspace.join("release.lock.toml");
    let lock_toml = toml::to_string_pretty(&lock).context("serialize release lock")?;
    tokio::fs::write(&lock_path, &lock_toml)
        .await
        .with_context(|| format!("write {}", lock_path.display()))?;
    println!("\n✅ release.lock.toml 已写入: {}", lock_path.display());
    println!(
        "   现在可以: APP_CLI_RUN_PROFILE={profile} app-cli run --workspace {} --pingap-bin <pingap>",
        workspace.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 防漂移守卫（单一版本源三方绑定，CI 在此锚定）：根
    /// [workspace.dependencies] 的 pingap-config 版本、本文件回退常量
    /// （版本 + commit）、tools/build/pingap-assets.json 官方资产目录条目
    /// 必须一致——升级 pingap 时三处（连同镜像 PINGAP_VERSION 注入）一起改。
    /// Python 门禁 tools/build/pingap_identity.py 校验同一契约。
    #[test]
    fn pingap_constants_match_workspace_pin_and_official_catalog() {
        let workspace_toml = include_str!("../../../../Cargo.toml");
        let pin_line = workspace_toml
            .lines()
            .find(|l| l.trim_start().starts_with("pingap-config"))
            .expect("根 Cargo.toml 应有 pingap-config 版本声明");
        let version = pin_line
            .split('"')
            .nth(1)
            .expect("pingap-config 行应为 \"<semver>\" 字符串形态");
        assert_eq!(
            version, DEFAULT_PINGAP_VERSION,
            "根 workspace 的 pingap-config 版本与 devtool DEFAULT_PINGAP_VERSION 漂移——\
             升级 pingap 时两处必须同步修改"
        );
        let catalog: serde_json::Value =
            serde_json::from_str(include_str!("../../../../tools/build/pingap-assets.json"))
                .expect("官方资产目录 JSON 合法");
        let release = &catalog["releases"][DEFAULT_PINGAP_VERSION];
        assert_eq!(
            release["tag"],
            format!("v{DEFAULT_PINGAP_VERSION}"),
            "官方资产目录缺 v{DEFAULT_PINGAP_VERSION} 条目或 tag 不匹配"
        );
        assert_eq!(
            release["commit"], DEFAULT_PINGAP_COMMIT,
            "官方资产目录 commit 与 devtool DEFAULT_PINGAP_COMMIT 漂移"
        );
    }
}

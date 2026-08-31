//! workspace 磁盘卫生：构建制品与日志的保留策略清理（构建完成后顺带触发）。
//!
//! 策略（默认值见 file-server Config，env 可调）：
//! - `builds/workspace-package-*.zip`：按文件名字典序（= uuid v7 时间序）保留
//!   最新 `retain_count` 个，其余删除（`.part` 半成品跳过——失败现场）。
//! - `logs/{service_id}/` 与 dev server 日志目录的 `dev-temp-*.log`：同样按
//!   文件名时间戳保留最新 `retain_count` 个。
//! - `dev-{YYYY-MM-DD}.log`：按文件名日期删除 `retention_days` 前的（解析失败
//!   跳过，保守不删）。
//! - `.staging/`：run_dir 换入失败/中断留下的残留目录，清空。
//!
//! fail-safe：单项删除失败 warn 留痕继续；返回统计（info 留痕给观测）。

use std::path::Path;

use chrono::{Local, NaiveDate};

use super::run_dir::STAGING_DIR;
use super::{WORKSPACE_BUILDS_DIR, WORKSPACE_PACKAGE_PREFIX};

/// 一次 sweep 的清理统计。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepStats {
    /// 删除的制品 zip 数
    pub artifacts_removed: usize,
    /// 删除的 temp 日志数（构建 + dev server）
    pub temp_logs_removed: usize,
    /// 删除的过期 main 日志数
    pub main_logs_removed: usize,
    /// 清掉的 .staging 残留目录数
    pub staging_dirs_removed: usize,
}

/// 对一个 workspace 与其 dev server 日志目录执行保留策略清理。
///
/// `ws` = workspace 根（builds/、logs/、.staging/）；`dev_log_dir` =
/// `{log_base_dir}/{app_id}`（dev server 进程 main/temp 日志）。今日正在写的
/// 文件天然在保留集内（字典序最大/日期为今天）。
pub async fn sweep_workspace(
    ws: &Path,
    dev_log_dir: &Path,
    retain_count: usize,
    retention_days: usize,
) -> SweepStats {
    let artifacts_removed = sweep_named_prefix_dir(
        &ws.join(WORKSPACE_BUILDS_DIR),
        WORKSPACE_PACKAGE_PREFIX,
        ".zip",
        retain_count,
    )
    .await;
    let mut stats = SweepStats {
        artifacts_removed,
        ..Default::default()
    };
    // 构建日志：logs/ 下每个 service 子目录
    if let Ok(mut entries) = tokio::fs::read_dir(ws.join("logs")).await {
        let mut subdirs = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.path().is_dir() {
                subdirs.push(entry.path());
            }
        }
        for dir in subdirs {
            stats.temp_logs_removed += sweep_temp_logs(&dir, retain_count).await;
            stats.main_logs_removed += sweep_stale_main_logs(&dir, retention_days).await;
        }
    }
    // dev server 进程日志目录 + app-cli 编排器日志子目录（app-cli.log.{date}，
    // tracing daily 轮转——日期为文件名尾段，同样按天数清）
    stats.temp_logs_removed += sweep_temp_logs(dev_log_dir, retain_count).await;
    stats.main_logs_removed += sweep_stale_main_logs(dev_log_dir, retention_days).await;
    let orchestrator_dir = dev_log_dir.join("app-cli");
    stats.main_logs_removed +=
        sweep_dated_files(&orchestrator_dir, "app-cli.log.", retention_days).await;
    // .staging 残留（run_dir 换入失败遗留）
    stats.staging_dirs_removed = sweep_staging(&ws.join(STAGING_DIR)).await;
    stats
}

/// 目录下 `{prefix}*{suffix}` 文件按文件名排序保留最新 `retain_count` 个。
/// （uuid v7 simple / 毫秒时间戳均为定长 hex，字典序即时间序。）
async fn sweep_named_prefix_dir(
    dir: &Path,
    prefix: &str,
    suffix: &str,
    retain_count: usize,
) -> usize {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return 0;
    };
    let mut names: Vec<String> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if !entry.file_type().await.is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(prefix) && name.ends_with(suffix) {
            names.push(name);
        }
    }
    remove_all_but_latest(dir, names, retain_count).await
}

/// `dev-temp-*.log` 按文件名（内嵌毫秒时间戳）排序保留最新 N 个。
async fn sweep_temp_logs(dir: &Path, retain_count: usize) -> usize {
    sweep_named_prefix_dir(dir, "dev-temp-", ".log", retain_count).await
}

/// 排序去尾：删除字典序最小的（超出 retain_count 的）那些，返回删除数。
async fn remove_all_but_latest(dir: &Path, mut names: Vec<String>, retain_count: usize) -> usize {
    if names.len() <= retain_count {
        return 0;
    }
    names.sort();
    let to_remove = &names[..names.len() - retain_count];
    let mut removed = 0;
    for name in to_remove {
        match tokio::fs::remove_file(dir.join(name)).await {
            Ok(()) => removed += 1,
            Err(e) => tracing::warn!(error = %e, file = %name, "hygiene remove failed (skipped)"),
        }
    }
    removed
}

/// `dev-{YYYY-MM-DD}.log` 按文件名日期删除 `retention_days` 天前的。
async fn sweep_stale_main_logs(dir: &Path, retention_days: usize) -> usize {
    sweep_dated_files(dir, "dev-", retention_days).await
}

/// `{prefix}{YYYY-MM-DD}`（日期为文件名尾段，可选 .log 后缀）按日期删除
/// `retention_days` 天前的；非该形态跳过（保守不删）。
async fn sweep_dated_files(dir: &Path, prefix: &str, retention_days: usize) -> usize {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return 0;
    };
    let cutoff = Local::now().date_naive() - chrono::Duration::days(retention_days as i64);
    let mut removed = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if !entry.file_type().await.is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(date) = parse_dated_name(&name, prefix) else {
            continue;
        };
        if date >= cutoff {
            continue;
        }
        match tokio::fs::remove_file(entry.path()).await {
            Ok(()) => removed += 1,
            Err(e) => {
                tracing::warn!(error = %e, file = %name, "hygiene remove stale main log failed (skipped)")
            }
        }
    }
    removed
}

/// `{prefix}{YYYY-MM-DD}`（可选 .log 后缀）→ 解析日期；非该形态返回 None。
/// 兼容两形态：`dev-2026-08-31.log`（构建/dev server main）与
/// `app-cli.log.2026-08-31`（app-cli tracing daily 轮转）。
fn parse_dated_name(name: &str, prefix: &str) -> Option<NaiveDate> {
    let rest = name.strip_prefix(prefix)?;
    let date_str = rest.strip_suffix(".log").unwrap_or(rest);
    NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
        .ok()
        .or_else(|| {
            // 容错：某些生成器可能带时间后缀，取前 10 位日期再试
            date_str
                .get(..10)
                .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        })
}

/// `.staging/` 目录清空（换入失败残留；成功换入后本就为空）。
async fn sweep_staging(staging_root: &Path) -> usize {
    let Ok(mut entries) = tokio::fs::read_dir(staging_root).await else {
        return 0;
    };
    let mut removed = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if !entry.path().is_dir() {
            continue;
        }
        match tokio::fs::remove_dir_all(entry.path()).await {
            Ok(()) => removed += 1,
            Err(e) => tracing::warn!(error = %e, "hygiene remove staging dir failed (skipped)"),
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), "x").expect("touch");
    }

    #[tokio::test]
    async fn artifacts_keep_latest_ten_ignoring_part() {
        let ws = tempfile::tempdir().expect("ws");
        let builds = ws.path().join(WORKSPACE_BUILDS_DIR);
        std::fs::create_dir_all(&builds).expect("builds");
        for i in 0..12 {
            touch(
                &builds,
                &format!("{WORKSPACE_PACKAGE_PREFIX}rel-{i:02}.zip"),
            );
        }
        touch(&builds, "workspace-package-half.part");
        touch(&builds, "readme.txt");
        let removed = sweep_named_prefix_dir(&builds, WORKSPACE_PACKAGE_PREFIX, ".zip", 10).await;
        assert_eq!(removed, 2);
        // 保留字典序最大 10 个（rel-02..rel-11）
        assert!(
            builds
                .join(format!("{WORKSPACE_PACKAGE_PREFIX}rel-11.zip"))
                .is_file()
        );
        assert!(
            builds
                .join(format!("{WORKSPACE_PACKAGE_PREFIX}rel-02.zip"))
                .is_file()
        );
        assert!(
            !builds
                .join(format!("{WORKSPACE_PACKAGE_PREFIX}rel-01.zip"))
                .exists()
        );
        assert!(
            !builds
                .join(format!("{WORKSPACE_PACKAGE_PREFIX}rel-00.zip"))
                .exists()
        );
        // .part 与噪音文件不动
        assert!(builds.join("workspace-package-half.part").is_file());
        assert!(builds.join("readme.txt").is_file());
    }

    #[tokio::test]
    async fn temp_logs_keep_latest_n_by_timestamp_name() {
        let dir = tempfile::tempdir().expect("dir");
        for ms in [1000u64, 2000, 3000] {
            touch(dir.path(), &format!("dev-temp-{ms}.log"));
        }
        let removed = sweep_temp_logs(dir.path(), 2).await;
        assert_eq!(removed, 1);
        assert!(!dir.path().join("dev-temp-1000.log").exists());
        assert!(dir.path().join("dev-temp-3000.log").is_file());
    }

    #[tokio::test]
    async fn main_logs_removed_by_filename_date() {
        let dir = tempfile::tempdir().expect("dir");
        let old = (Local::now().date_naive() - chrono::Duration::days(10))
            .format("%Y-%m-%d")
            .to_string();
        let recent = (Local::now().date_naive() - chrono::Duration::days(2))
            .format("%Y-%m-%d")
            .to_string();
        let today = Local::now().date_naive().format("%Y-%m-%d").to_string();
        touch(dir.path(), &format!("dev-{old}.log"));
        touch(dir.path(), &format!("dev-{recent}.log"));
        touch(dir.path(), &format!("dev-{today}.log"));
        touch(dir.path(), "dev-weird-name.log");
        let removed = sweep_stale_main_logs(dir.path(), 7).await;
        assert_eq!(removed, 1);
        assert!(!dir.path().join(format!("dev-{old}.log")).exists());
        assert!(dir.path().join(format!("dev-{recent}.log")).is_file());
        assert!(dir.path().join(format!("dev-{today}.log")).is_file());
        assert!(dir.path().join("dev-weird-name.log").is_file());
    }

    #[tokio::test]
    async fn orchestrator_daily_logs_removed_by_date() {
        let dev_dir = tempfile::tempdir().expect("dev dir");
        let orch = dev_dir.path().join("app-cli");
        std::fs::create_dir_all(&orch).expect("orch dir");
        let old = (Local::now().date_naive() - chrono::Duration::days(10))
            .format("%Y-%m-%d")
            .to_string();
        let recent = Local::now().date_naive().format("%Y-%m-%d").to_string();
        std::fs::write(orch.join(format!("app-cli.log.{old}")), "{}").expect("old");
        std::fs::write(orch.join(format!("app-cli.log.{recent}")), "{}").expect("recent");
        std::fs::write(orch.join("app-cli.out.log"), "x").expect("out");
        let removed = sweep_dated_files(&orch, "app-cli.log.", 7).await;
        assert_eq!(removed, 1);
        assert!(!orch.join(format!("app-cli.log.{old}")).exists());
        assert!(orch.join(format!("app-cli.log.{recent}")).is_file());
        // 非 dated 形态（out/err 合流）不受影响
        assert!(orch.join("app-cli.out.log").is_file());
    }

    #[tokio::test]
    async fn staging_residual_dirs_are_removed() {
        let ws = tempfile::tempdir().expect("ws");
        let staging = ws.path().join(STAGING_DIR);
        std::fs::create_dir_all(staging.join("rel-1")).expect("s1");
        std::fs::create_dir_all(staging.join("rel-2")).expect("s2");
        std::fs::write(staging.join("rel-1").join("x"), "1").expect("f");
        assert_eq!(sweep_staging(&staging).await, 2);
        assert!(!staging.join("rel-1").exists());
        assert!(!staging.join("rel-2").exists());
    }

    #[tokio::test]
    async fn sweep_workspace_end_to_end() {
        let ws = tempfile::tempdir().expect("ws");
        let builds = ws.path().join(WORKSPACE_BUILDS_DIR);
        std::fs::create_dir_all(&builds).expect("builds");
        for i in 0..3 {
            touch(&builds, &format!("{WORKSPACE_PACKAGE_PREFIX}r{i}.zip"));
        }
        let logs = ws.path().join("logs").join("api");
        std::fs::create_dir_all(&logs).expect("logs");
        touch(&logs, "dev-temp-1.log");
        touch(&logs, "dev-temp-2.log");
        let staging = ws.path().join(STAGING_DIR).join("residual");
        std::fs::create_dir_all(&staging).expect("staging");
        let dev_log = tempfile::tempdir().expect("dev log");
        touch(dev_log.path(), "dev-temp-9.log");

        let stats = sweep_workspace(ws.path(), dev_log.path(), 2, 7).await;
        assert_eq!(stats.artifacts_removed, 1);
        assert_eq!(stats.temp_logs_removed, 0); // 各目录内未超 N
        assert_eq!(stats.staging_dirs_removed, 1);
        assert!(
            builds
                .join(format!("{WORKSPACE_PACKAGE_PREFIX}r2.zip"))
                .is_file()
        );
        assert!(
            !builds
                .join(format!("{WORKSPACE_PACKAGE_PREFIX}r0.zip"))
                .exists()
        );
    }
}

//! dev server 终止: stop_dev / shutdown_all / Drop 兜底。

use std::collections::HashSet;

use super::log;
use super::process;
use super::support::lock;
use super::types::{DevServerManager, StoppedDev};
use crate::error::AppResult;
use crate::models::KilledPid;

impl DevServerManager {
    /// stop-dev (对齐 nuwax stopDevServerByProjectId; 系统级 pid 扫描 + 杀整组 +
    /// 释放端口 + 清 temp 日志)。候选 pid = 内存 Map pid ∪ `ps` 扫描 pid (去重)。
    pub async fn stop_dev(&self, project_id: &str) -> AppResult<StoppedDev> {
        let proc = lock(&self.processes)?.remove(project_id);
        // 候选 pid: 内存 Map + 系统扫描 (去重)
        let mut candidates: Vec<u32> = Vec::new();
        if let Some(p) = &proc {
            candidates.push(p.pid);
        }
        candidates.extend(process::find_pids_by_project_id(project_id).await);
        candidates.sort_unstable();
        candidates.dedup();

        let candidates: Vec<(u32, Option<u32>)> = candidates
            .into_iter()
            .map(|pid| (pid, process::process_group_id(pid)))
            .collect();
        let mut stopped_groups = HashSet::new();
        let mut killed: Vec<KilledPid> = Vec::new();
        for (pid, pgid) in candidates {
            // 第一个成员已通过 kill(-pgid) 停止整组，其余成员不应
            // 因无法再次发送信号而被误报为 false。
            if pgid.is_some_and(|group| stopped_groups.contains(&group)) {
                killed.push(KilledPid { pid, killed: true });
                continue;
            }
            let ok = process::kill_process_group(pid);
            process::wait_for_stop(
                pid,
                self.config.dev_stop_check_interval_ms,
                self.config.dev_stop_max_attempts,
            )
            .await;
            let mut k = ok;
            if process::is_process_running(pid) {
                tracing::warn!("dev server (pid {pid}) 未在 SIGTERM 宽限期退出, 升级 SIGKILL");
                let force_sent = process::kill_process_group_force(pid);
                process::wait_for_stop(
                    pid,
                    self.config.dev_stop_check_interval_ms,
                    self.config.dev_stop_max_attempts,
                )
                .await;
                // zombie 进程在父进程回收前 `kill(pid, 0)` 仍会返回存在，
                // 但 SIGKILL 已成功送达时业务上应视为 killed，对齐 nuwax killProcess。
                k = k || force_sent || !process::is_process_running(pid);
            }
            if k && let Some(group) = pgid {
                stopped_groups.insert(group);
            }
            killed.push(KilledPid { pid, killed: k });
        }
        if let Some(p) = proc {
            self.port_pool.release(project_id);
            log::cleanup_temp_logs(&p.log_dir).await;
        }
        Ok(StoppedDev {
            killed_pids: killed,
        })
    }

    /// 全量优雅停止 (供 main.rs graceful shutdown 调用):
    /// 逐个项目走完整 `stop_dev` 流程 (SIGTERM → 等 → SIGKILL + ps 扫描 + 还端口 + 清日志)。
    /// 幂等: 进程已不在也安全返回; 单项失败记 warn 不中断其余。
    pub async fn shutdown_all(&self) {
        let snapshot: Vec<String> = lock(&self.processes)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        if snapshot.is_empty() {
            return;
        }
        tracing::info!("shutdown_all: stopping {} dev server(s)", snapshot.len());
        for project_id in snapshot {
            if let Err(e) = self.stop_dev(&project_id).await {
                tracing::warn!(%project_id, "shutdown_all stop failed: {e}");
            }
        }
    }
}

/// Drop 兜底: 正常路径 (graceful shutdown) 已由 `shutdown_all` 清空实例表;
/// 此处只防 "panic / Arc 提前释放 / shutdown_all 未触发" 残留。
///
/// 约束: `Drop::drop` 不能 `.await`, 无法给 SIGTERM grace 宽限期 ——
/// 发 SIGTERM 后无等待地 SIGKILL 等价于直接 SIGKILL, 故此处省去无意义的 SIGTERM,
/// 直接对进程组 SIGKILL, 确保进程终止并还端口、清表。
///
/// ⚠️ 不覆盖场景: file-server 自身被 SIGKILL 强杀时进程直接终止, **Drop 不会执行**,
/// detached 的 dev server 仍会成孤儿 —— 这是 detached 模型的固有局限, 只能靠
/// 容器/编排层 (Pod 退出回收) 兜底。正常重启走 SIGTERM → `shutdown_all` 路径已覆盖。
impl Drop for DevServerManager {
    fn drop(&mut self) {
        let Ok(procs) = self.processes.get_mut() else {
            return;
        };
        if procs.is_empty() {
            return;
        }
        tracing::warn!(
            "DevServerManager dropped with {} live dev server(s) — best-effort SIGKILL",
            procs.len()
        );
        for (project_id, p) in procs.iter() {
            // 兜底硬杀: SIGKILL 进程组 (无法 await, 故不走 SIGTERM→等→SIGKILL 升级)
            if !process::kill_process_group_force(p.pid) {
                tracing::warn!(%project_id, pid = p.pid, "SIGKILL failed in Drop");
            }
            self.port_pool.release(project_id);
        }
        procs.clear();
    }
}

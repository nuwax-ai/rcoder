//! 内存级端口池 (对齐 nuwax `portPool.js`)。
//!
//! - 范围 + 保留区由 config 决定 (默认 4000-55000, 跳过 8000-9000)
//! - `allocate(projectId)`: 同 projectId 复用 (幂等), 否则取最小可用端口
//! - `release(projectId)`: 归还
//! - 不主动探测端口占用 (信任 Map 单源), 与 nuwax 一致
//! - 无持久化, 服务重启即清空 (容器内 dev server 寿命 = 进程寿命)

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::error::{AppError, AppResult};
use crate::models::PortAllocation;

/// 端口池快照 (供 port-pool-status 路由)。
#[derive(Debug, Clone, serde::Serialize)]
pub struct PortPoolStatus {
    pub port_range: String,
    pub total_allocated: usize,
    pub allocations: Vec<PortAllocation>,
}

/// 单例端口池 (经 Arc 共享于 DevServerManager)。
pub struct PortPool {
    allocated: Mutex<HashMap<String, u16>>,
    range_start: u16,
    range_end: u16,
    reserved_start: u16,
    reserved_end: u16,
}

impl PortPool {
    pub fn new(range_start: u16, range_end: u16, reserved_start: u16, reserved_end: u16) -> Self {
        Self {
            allocated: Mutex::new(HashMap::new()),
            range_start,
            range_end,
            reserved_start,
            reserved_end,
        }
    }

    /// 分配端口: 同 projectId 复用, 否则取最小可用端口。
    pub fn allocate(&self, project_id: &str) -> AppResult<u16> {
        let mut alloc = self
            .allocated
            .lock()
            .map_err(|e| AppError::system(format!("port pool lock: {e}")))?;
        // 幂等复用
        if let Some(&p) = alloc.get(project_id) {
            return Ok(p);
        }
        let taken: HashSet<u16> = alloc.values().copied().collect();
        for p in self.range_start..=self.range_end {
            if p >= self.reserved_start && p <= self.reserved_end {
                continue;
            }
            if !taken.contains(&p) {
                alloc.insert(project_id.to_string(), p);
                return Ok(p);
            }
        }
        Err(AppError::system(format!(
            "port pool exhausted (range {}-{})",
            self.range_start, self.range_end
        )))
    }

    /// 释放端口 (从 Map 移除)。返回被释放的端口 (无则 None)。
    pub fn release(&self, project_id: &str) -> Option<u16> {
        self.allocated.lock().ok()?.remove(project_id)
    }

    /// 当前分配快照。
    pub fn status(&self) -> AppResult<PortPoolStatus> {
        let alloc = self
            .allocated
            .lock()
            .map_err(|e| AppError::system(format!("port pool lock: {e}")))?;
        let mut allocations: Vec<PortAllocation> = alloc
            .iter()
            .map(|(pid, &port)| PortAllocation {
                project_id: pid.clone(),
                port,
            })
            .collect();
        allocations.sort_by_key(|a| a.port);
        Ok(PortPoolStatus {
            port_range: format!("{}-{}", self.range_start, self.range_end),
            total_allocated: allocations.len(),
            allocations,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> PortPool {
        // 小范围测试池: 4000-4010, 保留 4004-4006
        PortPool::new(4000, 4010, 4004, 4006)
    }

    #[test]
    fn allocate_reuses_same_project() {
        let p = pool();
        let a = p.allocate("proj-a").unwrap();
        let b = p.allocate("proj-a").unwrap();
        assert_eq!(a, b, "same projectId must reuse port");
    }

    #[test]
    fn allocate_skips_reserved_range() {
        let p = pool();
        let a = p.allocate("a").unwrap(); // 4000
        let b = p.allocate("b").unwrap(); // 4001
        let c = p.allocate("c").unwrap(); // 4002
        let cc = p.allocate("cc").unwrap(); // 4003
        let e = p.allocate("e").unwrap(); // 4007 (跳过 4004-4006)
        assert_eq!(a, 4000);
        assert_eq!(b, 4001);
        assert_eq!(c, 4002);
        assert_eq!(cc, 4003);
        assert_eq!(e, 4007, "4004-4006 reserved, next free after 4003 is 4007");
    }

    #[test]
    fn release_returns_port_to_pool() {
        let p = pool();
        let a = p.allocate("a").unwrap();
        assert_eq!(p.release("a"), Some(a));
        // 再分配应重新拿到同一最小端口
        let a2 = p.allocate("a").unwrap();
        assert_eq!(a2, a);
    }
}

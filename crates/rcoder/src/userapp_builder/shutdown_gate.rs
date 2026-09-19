//! 在途协调任务门闸（R02 关机顺序）：收到 SIGTERM 后先停生产者、
//! 等待在途业务协调任务在有界预算内收束，再关闭控制存储——排空数据
//! 库队列不等于业务已结束（任务可能正在容器操作之间，稍后要提交终态）。
//!
//! 覆盖 spawn 出去的 userApp 协调任务（创建/停止/重启工作器）。HTTP
//! 在途请求由 server accept 循环停止接单，其 handler 内联调用不经本
//! 门闸（本轮范围，见验证报告）。

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

/// 门闸：活动计数 + 有界轮询等待。轮询而非 Notify 避免 permit 丢失的
/// 竞态；协调任务数量小，轮询开销可忽略。
#[derive(Default)]
pub struct OperationFlightGate {
    active: AtomicUsize,
}

pub struct FlightGuard {
    gate: Arc<OperationFlightGate>,
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        self.gate.active.fetch_sub(1, Ordering::Release);
    }
}

impl OperationFlightGate {
    pub fn guard(self: &Arc<Self>) -> FlightGuard {
        self.active.fetch_add(1, Ordering::AcqRel);
        FlightGuard {
            gate: Arc::clone(self),
        }
    }

    fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// 等待在途任务收束；返回超时时刻仍活跃的数量（0 = 全部收束）。
    pub async fn wait_idle(&self, budget: Duration) -> usize {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let active = self.active();
            if active == 0 {
                return 0;
            }
            if tokio::time::Instant::now() >= deadline {
                return active;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn gate_reports_budget_overflow_and_idle() {
        let gate = Arc::new(OperationFlightGate::default());
        assert_eq!(gate.wait_idle(Duration::from_millis(10)).await, 0);
        // 预算耗尽路径：持有 guard 时短预算必返回剩余数量
        let held = gate.guard();
        assert_eq!(gate.active(), 1);
        assert_eq!(
            gate.wait_idle(Duration::from_millis(80)).await,
            1,
            "预算内未收束应返回剩余数量"
        );
        drop(held);
        assert_eq!(gate.wait_idle(Duration::from_millis(80)).await, 0);
    }
}

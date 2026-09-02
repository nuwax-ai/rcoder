//! Fuzz 目标: K8s Quantity 解析（winnow 实现）
//!
//! 覆盖 parse_memory_quantity / parse_cpu_quantity / validate_k8s_storage_size
//! 三个纯函数解析入口。约定: 任何输入不得 panic / 挂死 / 溢出中断——
//! 非法输入返回 None/Err 是合法路径，panic 即 bug。

#![no_main]

use libfuzzer_sys::fuzz_target;
use shared_types::{parse_cpu_quantity, parse_memory_quantity, validate_k8s_storage_size};

fuzz_target!(|data: &str| {
    let _ = parse_memory_quantity(data);
    let _ = parse_cpu_quantity(data);
    let _ = validate_k8s_storage_size(data);
});

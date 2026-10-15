//! Fuzz 目标: Chat 配置字符串解析
//!
//! 覆盖 AgentMode（yolo/ask）与 ToolApprovalAction（ask/allow/deny）两个
//! 枚举 parse 入口。约定: 未知取值返回 Err，任何输入不得 panic。

#![no_main]

use libfuzzer_sys::fuzz_target;
use shared_types::{AgentMode, ToolApprovalAction};
use std::str::FromStr;

fuzz_target!(|data: &str| {
    let _ = AgentMode::parse(Some(data));
    let _ = AgentMode::from_str(data);
    let _ = ToolApprovalAction::parse(data);
});

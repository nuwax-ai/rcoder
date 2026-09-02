//! Fuzz 目标: semver 版本字符串解析
//!
//! 覆盖 parse_semver / normalize_version。约定: 非法版本返回 None/Err，
//! 任何输入不得 panic。

#![no_main]

use libfuzzer_sys::fuzz_target;
use shared_types::version_util::{normalize_version, parse_semver};

fuzz_target!(|data: &str| {
    let _ = parse_semver(data);
    let _ = normalize_version(data);
});

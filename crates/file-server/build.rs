//! build.rs — 编译时注入版本信息 (git commit hash + branch + build time)
//!
//! 不依赖外部 crate, 纯标准库实现。
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let git_branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let git_dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| if s.trim().is_empty() { "" } else { "+dirty" }.to_string())
        .unwrap_or_default();

    // build time: Unix timestamp (纯标准库, 不依赖 time/chrono crate)
    let build_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    println!("cargo:rustc-env=RCODER_BUILD_GIT_HASH={git_hash}{git_dirty}");
    println!("cargo:rustc-env=RCODER_BUILD_GIT_BRANCH={git_branch}");
    println!("cargo:rustc-env=RCODER_BUILD_TIME={build_time}");
    println!("cargo:rerun-if-changed=.git/HEAD");
}

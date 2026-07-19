//! build.rs — 编译时注入版本信息 (git commit hash + branch + build time)
#![allow(deprecated)]
use std::process::Command;

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

    let fmt = time::format_description::parse("[year]-[month]-[day] [hour]:[minute]:[second] UTC")
        .unwrap_or_else(|_| time::format_description::parse("[unix_timestamp]").unwrap());
    let build_time = time::OffsetDateTime::now_utc()
        .format(&fmt)
        .unwrap_or_else(|_| "unknown".to_string());

    println!("cargo:rustc-env=RCODER_BUILD_GIT_HASH={git_hash}{git_dirty}");
    println!("cargo:rustc-env=RCODER_BUILD_GIT_BRANCH={git_branch}");
    println!("cargo:rustc-env=RCODER_BUILD_TIME={build_time}");
    println!("cargo:rerun-if-changed=.git/HEAD");
}

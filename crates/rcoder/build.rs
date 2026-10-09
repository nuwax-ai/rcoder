//! 编译时记录版本来源；冻结构建使用明确输入，开发环境读取 Git。
use std::env;
use std::io;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn git_output(arguments: &[&str]) -> Option<String> {
    let output = Command::new("git").args(arguments).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_owned())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let explicit_commit = match env::var("RCODER_SOURCE_COMMIT") {
        Ok(value) => {
            if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(io::Error::other(
                    "RCODER_SOURCE_COMMIT must be an exact 40-character Git commit",
                )
                .into());
            }
            Some(value)
        }
        Err(env::VarError::NotPresent) => None,
        Err(error) => return Err(error.into()),
    };
    let git_hash = explicit_commit
        .clone()
        .or_else(|| git_output(&["rev-parse", "--short", "HEAD"]).filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "unknown".to_owned());
    let git_branch = match env::var("RCODER_SOURCE_BRANCH") {
        Ok(value) if !value.is_empty() && !value.contains(['\n', '\r']) => value,
        Ok(_) => {
            return Err(
                io::Error::other("RCODER_SOURCE_BRANCH must be a nonempty single line").into(),
            );
        }
        Err(env::VarError::NotPresent) if explicit_commit.is_some() => "frozen".to_owned(),
        Err(env::VarError::NotPresent) => git_output(&["rev-parse", "--abbrev-ref", "HEAD"])
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unknown".to_owned()),
        Err(error) => return Err(error.into()),
    };
    let git_dirty = if explicit_commit.is_some() {
        String::new()
    } else {
        git_output(&["status", "--porcelain"])
            .map(|value| if value.is_empty() { "" } else { "+dirty" }.to_owned())
            .unwrap_or_default()
    };
    let build_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=RCODER_BUILD_GIT_HASH={git_hash}{git_dirty}");
    println!("cargo:rustc-env=RCODER_BUILD_GIT_BRANCH={git_branch}");
    println!("cargo:rustc-env=RCODER_BUILD_TIME={build_time}");
    println!("cargo:rerun-if-env-changed=RCODER_SOURCE_COMMIT");
    println!("cargo:rerun-if-env-changed=RCODER_SOURCE_BRANCH");
    println!("cargo:rerun-if-changed=.git/HEAD");
    Ok(())
}

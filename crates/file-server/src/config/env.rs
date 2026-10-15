//! 环境变量解析工具与限额校验 (配置模块内部使用)。

use std::str::FromStr;

use anyhow::{Context, Result, anyhow};

/// 所有客户端上传文件共享的硬上限：1 GiB。
pub const MAX_UPLOAD_FILE_SIZE_BYTES: u64 = 1024 * 1024 * 1024;

pub(super) fn env_str(key: &str, default: &str) -> Result<String> {
    match std::env::var(key) {
        Ok(value) => Ok(value),
        Err(std::env::VarError::NotPresent) => Ok(default.to_string()),
        Err(error) => Err(anyhow!(error)).context(format!("read environment variable {key}")),
    }
}

/// 可选字符串 env：未设置 → None；设置但 trim 为空 → None（显式关闭）；否则 Some(trim)。
pub(super) fn env_opt_string(key: &str) -> Result<Option<String>> {
    match std::env::var(key) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(anyhow!(error)).context(format!("read environment variable {key}")),
    }
}

pub(super) fn env_bool(key: &str, default: bool) -> Result<bool> {
    match std::env::var(key) {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Ok(true),
            "false" | "0" | "no" => Ok(false),
            _ => Err(anyhow!(
                "environment variable {key} must be true/false, 1/0, or yes/no"
            )),
        },
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(anyhow!(error)).context(format!("read environment variable {key}")),
    }
}

pub(super) fn env_parse<T>(key: &str, default: T) -> Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(key) {
        Ok(value) => value
            .trim()
            .parse()
            .map_err(|error| anyhow!("invalid environment variable {key}: {error}")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(anyhow!(error)).context(format!("read environment variable {key}")),
    }
}

/// 解析带单位的字节数 (如 `2gb`/`100mb`/`1024`), 单位不区分大小写。
pub(super) fn parse_byte_size(value: &str) -> Option<u64> {
    let normalized = value.trim().to_ascii_lowercase();
    let (number, multiplier) = [
        ("gb", 1024_u64.pow(3)),
        ("mb", 1024_u64.pow(2)),
        ("kb", 1024_u64),
        ("b", 1_u64),
    ]
    .into_iter()
    .find_map(|(suffix, multiplier)| {
        normalized
            .strip_suffix(suffix)
            .map(|number| (number.trim(), multiplier))
    })
    .unwrap_or((normalized.as_str(), 1));
    number
        .parse::<u64>()
        .ok()
        .and_then(|value| value.checked_mul(multiplier))
}

/// 读取 Axum 请求体上限: 兼容历史变量名 `REQUEST_BODY_LIMIT`, 支持带单位值。
pub(super) fn request_body_max_bytes(default: u64) -> Result<u64> {
    for key in ["REQUEST_BODY_MAX_BYTES", "REQUEST_BODY_LIMIT"] {
        match std::env::var(key) {
            Ok(value) => {
                return parse_byte_size(&value)
                    .ok_or_else(|| anyhow!("invalid byte-size environment variable {key}"));
            }
            Err(std::env::VarError::NotPresent) => {}
            Err(error) => {
                return Err(anyhow!(error)).context(format!("read environment variable {key}"));
            }
        }
    }
    Ok(default)
}

pub(super) fn env_list(key: &str, default: &str) -> Result<Vec<String>> {
    Ok(env_str(key, default)?
        .split(',')
        .map(str::trim)
        .map(ToOwned::to_owned)
        .filter(|s| !s.is_empty())
        .collect())
}

pub(super) fn validate_upload_limit(value: u64) -> Result<()> {
    if value == 0 || value > MAX_UPLOAD_FILE_SIZE_BYTES {
        return Err(anyhow!(
            "UPLOAD_MAX_FILE_SIZE_BYTES must be between 1 and {MAX_UPLOAD_FILE_SIZE_BYTES} bytes (1 GiB)"
        ));
    }
    Ok(())
}

pub(super) fn validate_request_body_limit(value: u64) -> Result<()> {
    if value == 0 || value > MAX_UPLOAD_FILE_SIZE_BYTES {
        return Err(anyhow!(
            "REQUEST_BODY_MAX_BYTES/REQUEST_BODY_LIMIT must be between 1 and {MAX_UPLOAD_FILE_SIZE_BYTES} bytes (1 GiB)"
        ));
    }
    Ok(())
}

/// 附件上传白名单 (对齐 nuwax `projectRoutes.js` ATTACHMENT_ALLOWED_EXTENSIONS, 硬编码)。
pub(super) fn default_attachment_extensions() -> Vec<String> {
    [
        ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".txt", ".md", ".png", ".jpg",
        ".jpeg", ".gif", ".bmp", ".webp", ".svg", ".ico", ".avif", ".zip", ".rar", ".7z", ".tar",
        ".gz", ".csv", ".json", ".xml", ".mp4", ".mov", ".avi", ".wmv", ".flv", ".mp3", ".wav",
        ".ogg", ".m4a",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// 逗号分隔默认值拆分 (不做 trim, 保持原 nuwax 语义)。
pub(super) fn split_default(value: &str) -> Vec<String> {
    value.split(',').map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_legacy_request_body_limit_units() {
        assert_eq!(parse_byte_size("2000mb"), Some(2_097_152_000));
        assert_eq!(parse_byte_size("2 GB"), Some(2_147_483_648));
        assert_eq!(parse_byte_size("1024"), Some(1024));
        assert_eq!(parse_byte_size("invalid"), None);
    }

    #[test]
    fn upload_limit_is_at_most_one_gibibyte() {
        assert!(validate_upload_limit(MAX_UPLOAD_FILE_SIZE_BYTES).is_ok());
        assert!(validate_upload_limit(MAX_UPLOAD_FILE_SIZE_BYTES + 1).is_err());
        assert!(validate_upload_limit(0).is_err());
    }

    #[test]
    fn request_body_limit_is_at_most_one_gibibyte() {
        assert!(validate_request_body_limit(MAX_UPLOAD_FILE_SIZE_BYTES).is_ok());
        assert!(validate_request_body_limit(MAX_UPLOAD_FILE_SIZE_BYTES + 1).is_err());
        assert!(validate_request_body_limit(0).is_err());
    }
}

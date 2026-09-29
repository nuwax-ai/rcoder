//! 杂项工具（hash、编码、时间戳、run-id 校验）。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

pub(crate) fn hash_fixtures(fixtures: &Path) -> Result<BTreeMap<String, String>> {
    let mut hashes = BTreeMap::new();
    for file in [
        "react-vite-template.zip",
        "vue3-vite-template.zip",
        "skills-fixture.zip",
        "workspace-project.zip",
        "package-project.zip",
    ] {
        let path = fixtures.join(file);
        let bytes = fs::read(&path).with_context(|| format!("read fixture {}", path.display()))?;
        hashes.insert(file.to_string(), sha256(&bytes));
    }
    Ok(hashes)
}

pub(crate) fn endpoint(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

pub(crate) fn encode_query(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

pub(crate) fn json_pointer_escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub(crate) fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.to_string())
}

pub(crate) fn timestamp_for_id() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string()
}

pub(crate) fn timestamp_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub(crate) fn validate_run_id(value: &str) -> Result<String> {
    if value.is_empty()
        || value.len() > 100
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("run id must contain 1-100 ASCII letters, digits, '-' or '_' only");
    }
    Ok(value.to_string())
}

//! per-service 进程 spec（supervisord 托管引擎的服务启动契约）。
//!
//! server 编排时把每个服务的启动参数（argv/cwd/env/port）写入
//! `/run/app-cli/specs/{release_id}/{service_id}.toml`（tmpfs，0600——含 pingap
//! admin 凭证不落持久卷），supervisord 的 `[program:app-svc-{id}]` 以
//! `app-cli run-service {rid} {id}` 为 command，包装进程读 spec 组装 env 后
//! `exec` 服务本体（无包装层残留，进程树干净）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// spec 文件内容。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ServiceSpecFile {
    pub release_id: String,
    pub service_id: String,
    /// 服务工作目录（workspace 相对或绝对）。
    pub cwd: String,
    /// 服务命令（argv[0] 经 PATH 解析）。
    pub argv: Vec<String>,
    /// 服务声明 env（runtime 覆盖键由 run-service 追加，不写入文件）。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 服务端口（run-service 注入 PORT 用；pingap 等无端口服务可缺省）。
    #[serde(default)]
    pub port: Option<u16>,
}

/// spec 根目录（/run tmpfs；env 可覆盖供测试）。
pub(crate) fn spec_root() -> PathBuf {
    std::env::var_os("APP_CLI_SPEC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/run/app-cli/specs".into())
}

impl ServiceSpecFile {
    /// 写入 spec 文件（0600——可能含凭证）并清理其它代目录。
    pub(crate) fn write(&self) -> Result<PathBuf> {
        let dir = spec_root().join(&self.release_id);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let path = dir.join(format!("{}.toml", self.service_id));
        std::fs::write(
            &path,
            toml::to_string_pretty(self).context("serialize spec")?,
        )
        .with_context(|| format!("write {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("chmod 600 {}", path.display()))?;
        }
        Ok(path)
    }

    /// 读取并解析 spec 文件。
    pub(crate) fn load(release_id: &str, service_id: &str) -> Result<Self> {
        let path = spec_root()
            .join(release_id)
            .join(format!("{service_id}.toml"));
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("read service spec {}", path.display()))?;
        let spec: Self =
            toml::from_str(&content).with_context(|| format!("parse {}", path.display()))?;
        if spec.release_id != release_id || spec.service_id != service_id {
            bail!(
                "spec identity mismatch: file says {}/{} but requested {release_id}/{service_id}",
                spec.release_id,
                spec.service_id
            );
        }
        if spec.argv.is_empty() {
            bail!("spec {} has empty argv", path.display());
        }
        Ok(spec)
    }

    /// 清理指定代之外的所有 spec 目录（编排收敛：只保留当前代）。
    pub(crate) fn prune_other_generations(keep_release_id: &str) {
        let root = spec_root();
        let Ok(entries) = std::fs::read_dir(&root) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy() != keep_release_id {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    /// runtime 覆盖 env（对齐 builtin 引擎的注入语义，防手工 lock 篡改）。
    pub(crate) fn runtime_env_overrides(&self, log_dir: &Path) -> BTreeMap<String, String> {
        let mut overrides = BTreeMap::new();
        overrides.insert("HOSTNAME".to_string(), "0.0.0.0".to_string());
        if let Some(port) = self.port {
            overrides.insert("PORT".to_string(), port.to_string());
        }
        overrides.insert(
            "APP_LOG_DIR".to_string(),
            log_dir
                .join(&self.service_id)
                .to_string_lossy()
                .into_owned(),
        );
        overrides.insert("APP_SERVICE_ID".to_string(), self.service_id.clone());
        overrides.insert("APP_RELEASE_ID".to_string(), self.release_id.clone());
        overrides
    }
}

/// APP_CLI_SPEC_DIR 是进程级 env——涉及 set_var 的测试经此锁串行，防并行互抢目录。
#[cfg(test)]
pub(crate) static SPEC_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    // APP_CLI_SPEC_DIR 经 std::env::set_var 注入——多个文件系统测试并行会互抢，
    // 合并为单测试串行执行（其余纯逻辑测试不受影响）。
    fn sample(release_id: &str, service_id: &str) -> ServiceSpecFile {
        ServiceSpecFile {
            release_id: release_id.into(),
            service_id: service_id.into(),
            cwd: "/app/code/web".into(),
            argv: vec!["node".into(), "server.js".into()],
            env: BTreeMap::from([("NODE_ENV".to_string(), "production".to_string())]),
            port: Some(4200),
        }
    }

    #[test]
    fn roundtrip_identity_guard_and_prune() {
        let _guard = SPEC_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("APP_CLI_SPEC_DIR", dir.path()) };

        let spec = sample("rel-a", "web");
        let path = spec.write().unwrap();
        assert!(path.ends_with("web.toml"));

        let loaded = ServiceSpecFile::load("rel-a", "web").unwrap();
        assert_eq!(
            loaded.argv,
            vec!["node".to_string(), "server.js".to_string()]
        );
        assert_eq!(loaded.port, Some(4200));

        // 身份不匹配拒绝（防串代读错文件）
        assert!(ServiceSpecFile::load("rel-b", "web").is_err());

        // 代际清理：只保留当前代
        sample("rel-old", "web").write().unwrap();
        sample("rel-new", "web").write().unwrap();
        ServiceSpecFile::prune_other_generations("rel-new");
        assert!(ServiceSpecFile::load("rel-new", "web").is_ok());
        assert!(ServiceSpecFile::load("rel-old", "web").is_err());
    }

    #[test]
    fn runtime_overrides_shape() {
        let spec = sample("rel-x", "api");
        let overrides = spec.runtime_env_overrides(Path::new("/app/logs"));
        assert_eq!(overrides.get("PORT").map(String::as_str), Some("4200"));
        assert_eq!(
            overrides.get("HOSTNAME").map(String::as_str),
            Some("0.0.0.0")
        );
        assert_eq!(
            overrides.get("APP_SERVICE_ID").map(String::as_str),
            Some("api")
        );
        assert_eq!(
            overrides.get("APP_RELEASE_ID").map(String::as_str),
            Some("rel-x")
        );
        assert_eq!(
            overrides.get("APP_LOG_DIR").map(String::as_str),
            Some("/app/logs/api")
        );
    }
}

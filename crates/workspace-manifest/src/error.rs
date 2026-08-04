#[derive(Debug)]
pub enum ManifestError {
    Parse(String),
    Validation(String),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(message) => write!(f, "parse manifest: {message}"),
            Self::Validation(message) => write!(f, "invalid manifest: {message}"),
        }
    }
}

impl std::error::Error for ManifestError {}

#[derive(Debug)]
pub enum DiscoverError {
    ReadDir { path: String, source: String },
    Io(String),
    ReadManifest { path: String, source: String },
    ParseManifest { path: String, source: String },
    Validation(String),
}

impl std::fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadDir { path, source } => write!(f, "read dir {path}: {source}"),
            Self::Io(message) => write!(f, "{message}"),
            Self::ReadManifest { path, source } => write!(f, "read {path}: {source}"),
            Self::ParseManifest { path, source } => write!(f, "parse {path}: {source}"),
            Self::Validation(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for DiscoverError {}

/// 加载 `release.lock.toml` 时的版本感知错误。
///
/// `load_release_lock` 是 release lock 的唯一读取入口；它按 `schema_version`
/// 分发到当前版本或历史版本的迁移链。本枚举覆盖读取链上所有失败模式，
/// 包括未来 C 类"不可推导、需重锁"的破坏性变更（[`LoadError::RequiresRebuild`]）。
#[derive(Debug)]
pub enum LoadError {
    /// TOML 语法或类型反序列化失败。
    Parse(String),
    /// lock 的 `schema_version` 比当前平台已知版本更新（老 app-cli 读新 lock）。
    /// 正常应被 release lock 的 `minimum_app_cli_version` 门禁提前拦截；
    /// 到这里说明门禁被绕过。
    NewerThanKnown { got: u32, known: u32 },
    /// `schema_version` 既非当前版本，也非任何已注册的历史版本。
    UnknownVersion(u32),
    /// 反序列化成功但运行时不变量不满足（如 services 为空）。
    Invariant(String),
    /// 破坏性变更无法仅从老 lock 推导，必须用源 manifest 重锁（Stage 2 路径）。
    /// app-cli 据此上报，平台侧触发 `relock_from_package`。
    RequiresRebuild {
        from: u32,
        to: u32,
        reason: String,
    },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(message) => write!(f, "parse release lock: {message}"),
            Self::NewerThanKnown { got, known } => {
                write!(f, "release lock schema_version {got} is newer than known {known}")
            }
            Self::UnknownVersion(version) => {
                write!(f, "unknown release lock schema_version {version}")
            }
            Self::Invariant(message) => write!(f, "invalid release lock: {message}"),
            Self::RequiresRebuild { from, to, reason } => {
                write!(f, "release lock v{from}→v{to} requires rebuild: {reason}")
            }
        }
    }
}

impl std::error::Error for LoadError {}

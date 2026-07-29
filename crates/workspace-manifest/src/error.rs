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

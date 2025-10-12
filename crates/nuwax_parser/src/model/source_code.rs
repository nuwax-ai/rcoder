use serde::{Deserialize, Serialize};
use derive_builder::Builder;
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, Builder, ToSchema)]
#[builder(
    default,
    setter(into, strip_option),
    build_fn(error = "derive_builder::UninitializedFieldError")
)]
#[derive(Default)]
pub struct ProjectSourceCode {
    pub files: Vec<FileInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Builder, ToSchema)]
#[builder(
    default,
    setter(into, strip_option),
    build_fn(error = "derive_builder::UninitializedFieldError")
)]
#[derive(Default)]
pub struct FileInfo {
    pub name: String,
    pub binary: bool,
    #[serde(rename = "sizeExceeded")]
    pub size_exceeded: bool,
    #[builder(default)]
    pub contents: Option<String>,
}



impl FileInfo {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            binary: false,
            size_exceeded: false,
            contents: None,
        }
    }

    pub fn with_contents(mut self, contents: impl Into<String>) -> Self {
        self.contents = Some(contents.into());
        self
    }

    pub fn binary(mut self, binary: bool) -> Self {
        self.binary = binary;
        self
    }

    pub fn size_exceeded(mut self, size_exceeded: bool) -> Self {
        self.size_exceeded = size_exceeded;
        self
    }
}

impl ProjectSourceCode {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_file(mut self, file: FileInfo) -> Self {
        self.files.push(file);
        self
    }

    pub fn with_files(mut self, files: Vec<FileInfo>) -> Self {
        self.files = files;
        self
    }
}
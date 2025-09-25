use crate::model::{FileInfo, ProjectSourceCode};
use anyhow::Result;
use content_inspector::ContentType;
use content_inspector::inspect;
use derive_builder::Builder;
use file_format::FileFormat;
use regex::Regex;
use std::fs;
use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::error;
use walkdir::WalkDir;

#[derive(Debug, Clone, Builder)]
#[builder(
    default,
    setter(into, strip_option),
    build_fn(error = "derive_builder::UninitializedFieldError")
)]
pub struct ProjectReadConfig {
    #[builder(default = "default_exclude_files()")]
    pub exclude_files: Vec<String>,
    #[builder(default = "default_exclude_dirs()")]
    pub exclude_dirs: Vec<String>,
    #[builder(default = "default_exclude_file_patterns()")]
    pub exclude_file_patterns: Vec<String>,
    #[builder(default = "default_exclude_dir_patterns()")]
    pub exclude_dir_patterns: Vec<String>,
    #[builder(setter(strip_option))]
    pub max_file_size: Option<u64>,
    pub include_hidden_dirs: bool,
    pub include_hidden_files: bool,
}

impl Default for ProjectReadConfig {
    fn default() -> Self {
        Self {
            exclude_files: default_exclude_files(),
            exclude_dirs: default_exclude_dirs(),
            exclude_file_patterns: default_exclude_file_patterns(),
            exclude_dir_patterns: default_exclude_dir_patterns(),
            max_file_size: None,
            include_hidden_dirs: false,
            include_hidden_files: false,
        }
    }
}

fn default_exclude_files() -> Vec<String> {
    vec![
        "CLAUDE.md".to_string(),
        "node_modules".to_string(),
        ".git".to_string(),
        "target".to_string(),
        "dist".to_string(),
        "build".to_string(),
    ]
}

fn default_exclude_dirs() -> Vec<String> {
    vec![
        ".git".to_string(),
        "node_modules".to_string(),
        "target".to_string(),
        "dist".to_string(),
        "build".to_string(),
    ]
}

fn default_exclude_file_patterns() -> Vec<String> {
    vec![r".*\.lock$".to_string(), r".*\.log$".to_string()]
}

fn default_exclude_dir_patterns() -> Vec<String> {
    vec![
        r"\..*".to_string(), // 匹配所有以 "." 开头的隐藏目录
    ]
}

#[derive(Debug, Error)]
pub enum ProjectReadError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Path does not exist: {0}")]
    PathNotFound(PathBuf),
    #[error("Invalid path: {0}")]
    InvalidPath(PathBuf),
}

pub struct ProjectReader {
    config: ProjectReadConfig,
    compiled_file_patterns: Vec<regex::Regex>,
    compiled_dir_patterns: Vec<regex::Regex>,
}

impl ProjectReader {
    pub fn new() -> Self {
        Self::with_config(ProjectReadConfig::default())
    }

    pub fn with_config(config: ProjectReadConfig) -> Self {
        let compiled_file_patterns = Self::compile_patterns(&config.exclude_file_patterns);
        let compiled_dir_patterns = Self::compile_patterns(&config.exclude_dir_patterns);

        Self {
            config,
            compiled_file_patterns,
            compiled_dir_patterns,
        }
    }

    fn compile_patterns(patterns: &[String]) -> Vec<Regex> {
        patterns
            .iter()
            .filter_map(|pattern| Regex::new(pattern).ok())
            .collect()
    }

    /// 扫描目录
    pub fn read_project(&self, project_path: impl AsRef<Path>) -> Result<ProjectSourceCode> {
        let project_path = project_path.as_ref();

        if !project_path.exists() {
            return Err(anyhow::anyhow!(
                "Path does not exist: {}",
                project_path.display()
            ));
        }

        if !project_path.is_dir() {
            return Err(anyhow::anyhow!("Invalid path: {}", project_path.display()));
        }

        let mut files = Vec::new();
        self.scan_directory_with_walkdir(project_path, &mut files)?;

        Ok(ProjectSourceCode::new().with_files(files))
    }

    /// 扫描目录
    fn scan_directory_with_walkdir(
        &self,
        base_path: &Path,
        files: &mut Vec<FileInfo>,
    ) -> Result<(), ProjectReadError> {
        let walker = WalkDir::new(base_path)
            .into_iter()
            .filter_entry(|e| !self.should_skip_walkdir_entry(e))
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file());

        for entry in walker {
            let path = entry.path();
            let file_name = entry.file_name().to_string_lossy();

            if let Ok(file_info) = self.process_file(base_path, path, &file_name) {
                files.push(file_info);
            }
        }

        Ok(())
    }

    fn should_skip_walkdir_entry(&self, entry: &walkdir::DirEntry) -> bool {
        let _path = entry.path();
        let name = entry.file_name().to_string_lossy();
        let depth = entry.depth();

        // Always include the root directory
        if depth == 0 {
            return false;
        }

        // Skip files based on exclusion list
        if entry.file_type().is_file() && self.config.exclude_files.contains(&name.to_string()) {
            return true;
        }

        // Skip directories based on exclusion list
        if entry.file_type().is_dir() && self.config.exclude_dirs.contains(&name.to_string()) {
            return true;
        }

        // Skip hidden directories if not included
        if entry.file_type().is_dir() && name.starts_with('.') && !self.config.include_hidden_dirs {
            return true;
        }

        // Skip hidden files if not included
        if entry.file_type().is_file() && name.starts_with('.') && !self.config.include_hidden_files
        {
            return true;
        }

        // Skip files based on regex patterns
        if entry.file_type().is_file() {
            for pattern in &self.compiled_file_patterns {
                if pattern.is_match(&name) {
                    return true;
                }
            }
        }

        // Skip directories based on regex patterns
        if entry.file_type().is_dir() {
            for pattern in &self.compiled_dir_patterns {
                if pattern.is_match(&name) {
                    return true;
                }
            }
        }

        false
    }

    fn process_file(
        &self,
        base_path: &Path,
        file_path: &Path,
        file_name: &str,
    ) -> Result<FileInfo> {
        let relative_path = file_path
            .strip_prefix(base_path)
            .unwrap_or(file_path)
            .to_string_lossy()
            .replace('\\', "/");

        let metadata = fs::metadata(file_path)?;
        let file_size = metadata.len();
        let size_exceeded = self
            .config
            .max_file_size
            .map_or(false, |max| file_size > max);

        let binary = self.is_binary_file(file_path, file_name)?;

        let contents = if !binary && !size_exceeded {
            match fs::read_to_string(file_path) {
                Ok(content) => Some(content),
                Err(_) => None,
            }
        } else {
            None
        };

        let mut file_info = FileInfo::new(relative_path)
            .binary(binary)
            .size_exceeded(size_exceeded);

        if let Some(content) = contents {
            file_info = file_info.with_contents(content);
        }

        Ok(file_info)
    }

    fn is_binary_file(&self, path: &Path, _file_name: &str) -> Result<bool> {
        // 使用 file-format 库基于文件内容判断是否为二进制文件
        self.is_binary_by_content(path)
    }

    fn is_binary_by_content(&self, path: &Path) -> Result<bool> {
        // 使用 file-format 库检测文件类型
        match FileFormat::from_file(path) {
            Ok(file_format) => {
                // 根据具体的 FileFormat 判断是否为二进制文件
                match file_format {
                    FileFormat::ArbitraryBinaryData => {
                        // 对于无法识别的格式，使用回退逻辑检查空字节
                        self.check_binary_content_fallback(path)
                    }
                    _ => Ok(!self.is_text_format(&file_format)),
                }
            }
            Err(e) => {
                error!("Error detecting file format: {}", e);
                // 如果 file-format 检测失败，使用回退逻辑
                self.check_binary_content_fallback(path)
            }
        }
    }

    fn is_text_format(&self, file_format: &FileFormat) -> bool {
        // 根据具体的 FileFormat 判断是否为文本文件
        // 可以用文本编辑器打开、人类可读的文件格式返回 true
        match file_format {
            // 特殊格式
            FileFormat::Empty => true, // 空文件视为文本文件

            // 基本文本格式
            FileFormat::PlainText => true,
            FileFormat::ExtensibleMarkupLanguage => true,
            FileFormat::HypertextMarkupLanguage => true,
            FileFormat::Latex => true,

            // 编程语言源代码
            FileFormat::PythonScript => true,
            FileFormat::LuaScript => true,
            FileFormat::RubyScript => true,
            FileFormat::PerlScript => true,
            FileFormat::ShellScript => true,
            FileFormat::ClojureScript => true,
            FileFormat::ToolCommandLanguageScript => true,

            // 其他文本相关格式
            FileFormat::GeographyMarkupLanguage => true,
            FileFormat::IndesignMarkupLanguage => true,
            FileFormat::KeyholeMarkupLanguage => true,
            FileFormat::MathematicalMarkupLanguage => true,
            FileFormat::TimedTextMarkupLanguage => true,

            // 未能识别的格式，需要回退检查
            FileFormat::ArbitraryBinaryData => false, // 这个情况在 is_binary_by_content 中单独处理

            // 其他所有格式都视为二进制文件
            _ => false,
        }
    }

    fn check_binary_content_fallback(&self, path: &Path) -> Result<bool> {
        // 回退检测逻辑：检查空字节来判断是否为二进制文件
        if let Ok(mut file) = fs::File::open(path) {
            let mut buffer = [0; 1024];

            file.read(&mut buffer)?;
            let binary_flag = match inspect(&buffer) {
                ContentType::BINARY => true,
                _ => false,
            };
            return Ok(binary_flag);
        }

        Ok(false)
    }
}

impl Default for ProjectReader {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    #[test]
    fn test_project_reader_new() {
        let reader = ProjectReader::new();
        assert!(!reader.config.exclude_files.is_empty());
        assert!(!reader.config.exclude_dirs.is_empty());
    }

    #[test]
    fn test_config_builder() {
        let config = ProjectReadConfigBuilder::default()
            .exclude_files(vec!["test.txt".to_string()])
            .exclude_dirs(vec!["test_dir".to_string()])
            .exclude_file_patterns(vec![r".*\.tmp$".to_string()])
            .exclude_dir_patterns(vec![r".*\.idea$".to_string()])
            .max_file_size(2048u64)
            .include_hidden_dirs(true)
            .include_hidden_files(true)
            .build()
            .unwrap();

        assert!(config.exclude_files.contains(&"test.txt".to_string()));
        assert!(config.exclude_dirs.contains(&"test_dir".to_string()));
        assert!(
            config
                .exclude_file_patterns
                .contains(&r".*\.tmp$".to_string())
        );
        assert!(
            config
                .exclude_dir_patterns
                .contains(&r".*\.idea$".to_string())
        );
        assert_eq!(config.max_file_size, Some(2048));
        assert!(config.include_hidden_dirs);
        assert!(config.include_hidden_files);
    }

    #[test]
    fn test_is_binary_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path();
        let reader = ProjectReader::new();

        // 创建测试文件
        // PNG 文件（二进制）
        let png_file = temp_path.join("test.png");
        File::create(&png_file)
            .unwrap()
            .write_all(b"\x89PNG\x0D\x0A\x1A\x0A")
            .unwrap();
        assert!(reader.is_binary_file(&png_file, "test.png").unwrap());

        // 文本文件
        let text_file = temp_path.join("test.txt");
        File::create(&text_file)
            .unwrap()
            .write_all(b"This is plain text content")
            .unwrap();
        assert!(!reader.is_binary_file(&text_file, "test.txt").unwrap());

        // Rust 源码文件
        let rs_file = temp_path.join("test.rs");
        File::create(&rs_file)
            .unwrap()
            .write_all(b"fn main() { println!(\"Hello\"); }")
            .unwrap();
        assert!(!reader.is_binary_file(&rs_file, "test.rs").unwrap());

        // package-lock.json（实际上是 JSON 文本）
        let json_file = temp_path.join("package-lock.json");
        File::create(&json_file)
            .unwrap()
            .write_all(b"{\"name\": \"test\", \"version\": \"1.0.0\"}")
            .unwrap();
        assert!(
            !reader
                .is_binary_file(&json_file, "package-lock.json")
                .unwrap()
        );

        // 包含 null 字节的二进制文件
        let binary_file = temp_path.join("test.dat");
        let mut binary_content = Vec::new();
        for i in 0..100 {
            binary_content.push(if i % 15 == 0 { 0 } else { b'A' });
        }
        File::create(&binary_file)
            .unwrap()
            .write_all(&binary_content)
            .unwrap();
        assert!(reader.is_binary_file(&binary_file, "test.dat").unwrap());
    }

    #[test]
    fn test_read_project_with_temp_dir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path();

        // Create test files
        File::create(temp_path.join("main.rs"))
            .unwrap()
            .write_all(b"fn main() { println!(\"Hello\"); }")
            .unwrap();
        File::create(temp_path.join("README.md"))
            .unwrap()
            .write_all(b"# Test Project")
            .unwrap();
        File::create(temp_path.join(".gitignore"))
            .unwrap()
            .write_all(b"target/")
            .unwrap();

        // Create a subdirectory
        fs::create_dir(temp_path.join("src")).unwrap();
        File::create(temp_path.join("src/lib.rs"))
            .unwrap()
            .write_all(b"pub fn hello() -> &'static str { \"Hello\" }")
            .unwrap();

        // Create hidden directory
        fs::create_dir(temp_path.join(".hidden")).unwrap();
        File::create(temp_path.join(".hidden/secret.txt"))
            .unwrap()
            .write_all(b"secret")
            .unwrap();

        // Create CLAUDE.md (should be excluded)
        File::create(temp_path.join("CLAUDE.md"))
            .unwrap()
            .write_all(b"# Claude config")
            .unwrap();

        let reader = ProjectReader::new();
        let result = reader.read_project(temp_path).unwrap();

        // Check that files were read correctly
        let files = result.files;
        assert!(!files.is_empty());

        // Find specific files
        let main_rs = files.iter().find(|f| f.name == "main.rs").unwrap();
        assert!(!main_rs.binary);
        assert!(!main_rs.size_exceeded);
        assert_eq!(
            main_rs.contents,
            Some("fn main() { println!(\"Hello\"); }".to_string())
        );

        let readme = files.iter().find(|f| f.name == "README.md").unwrap();
        assert!(!readme.binary);
        assert_eq!(readme.contents, Some("# Test Project".to_string()));

        let lib_rs = files.iter().find(|f| f.name == "src/lib.rs").unwrap();
        assert_eq!(
            lib_rs.contents,
            Some("pub fn hello() -> &'static str { \"Hello\" }".to_string())
        );

        // Check that excluded files are not present
        assert!(!files.iter().any(|f| f.name == "CLAUDE.md"));
        assert!(!files.iter().any(|f| f.name.starts_with(".hidden")));
        assert!(!files.iter().any(|f| f.name == ".gitignore"));
    }

    #[test]
    fn test_with_custom_config() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path();

        // Create test files
        File::create(temp_path.join("main.rs"))
            .unwrap()
            .write_all(b"fn main() {}")
            .unwrap();
        File::create(temp_path.join("test.txt"))
            .unwrap()
            .write_all(b"test")
            .unwrap();
        File::create(temp_path.join("exclude.me"))
            .unwrap()
            .write_all(b"exclude")
            .unwrap();

        let config = ProjectReadConfigBuilder::default()
            .exclude_files(vec!["exclude.me".to_string()])
            .include_hidden_files(true)
            .build()
            .unwrap();

        let reader = ProjectReader::with_config(config);
        let result = reader.read_project(temp_path).unwrap();

        let files = result.files;
        assert!(files.iter().any(|f| f.name == "main.rs"));
        assert!(files.iter().any(|f| f.name == "test.txt"));
        assert!(!files.iter().any(|f| f.name == "exclude.me"));
    }

    #[test]
    fn test_exclude_files_with_config() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path();

        // Create test files
        File::create(temp_path.join("main.rs"))
            .unwrap()
            .write_all(b"fn main() {}")
            .unwrap();
        File::create(temp_path.join("test.txt"))
            .unwrap()
            .write_all(b"test content")
            .unwrap();
        File::create(temp_path.join("exclude.me"))
            .unwrap()
            .write_all(b"exclude this")
            .unwrap();
        File::create(temp_path.join("also_exclude.me"))
            .unwrap()
            .write_all(b"also exclude")
            .unwrap();

        let reader = ProjectReader::new();

        // Test with no excludes
        let result = reader.read_project(temp_path).unwrap();
        assert_eq!(result.files.len(), 4);

        // Test with excludes using config
        let config = ProjectReadConfigBuilder::default()
            .exclude_files(vec!["exclude.me".to_string()])
            .build()
            .unwrap();
        let exclude_reader = ProjectReader::with_config(config);

        let result = exclude_reader.read_project(temp_path).unwrap();
        assert_eq!(result.files.len(), 3);
        assert!(!result.files.iter().any(|f| f.name == "exclude.me"));
        assert!(result.files.iter().any(|f| f.name == "test.txt"));
        assert!(result.files.iter().any(|f| f.name == "main.rs"));
        assert!(result.files.iter().any(|f| f.name == "also_exclude.me"));

        // Test with multiple excludes using config
        let config = ProjectReadConfigBuilder::default()
            .exclude_files(vec![
                "exclude.me".to_string(),
                "also_exclude.me".to_string(),
            ])
            .build()
            .unwrap();
        let multi_exclude_reader = ProjectReader::with_config(config);

        let result = multi_exclude_reader.read_project(temp_path).unwrap();
        assert_eq!(result.files.len(), 2);
        assert!(!result.files.iter().any(|f| f.name == "exclude.me"));
        assert!(!result.files.iter().any(|f| f.name == "also_exclude.me"));
        assert!(result.files.iter().any(|f| f.name == "test.txt"));
        assert!(result.files.iter().any(|f| f.name == "main.rs"));
    }

    #[test]
    fn test_is_binary_by_content() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path();

        let reader = ProjectReader::new();

        // Test text file
        let text_file = temp_path.join("text.txt");
        File::create(&text_file)
            .unwrap()
            .write_all(b"This is plain text content")
            .unwrap();
        assert!(!reader.is_binary_file(&text_file, "text.txt").unwrap());

        // Test binary file with null bytes (会触发备用检测逻辑)
        let binary_file = temp_path.join("binary.dat");
        let mut binary_content = Vec::new();
        for i in 0..100 {
            binary_content.push(if i % 5 == 0 { 0 } else { b'A' }); // 20% null bytes
        }
        File::create(&binary_file)
            .unwrap()
            .write_all(&binary_content)
            .unwrap();
        // 注意：如果 file-format 不能识别这个文件为二进制，就会使用备用检测逻辑
        assert!(reader.is_binary_file(&binary_file, "binary.dat").unwrap());

        // Test PNG file signature (应该被 file-format 识别为二进制)
        let png_file = temp_path.join("fake.png");
        File::create(&png_file)
            .unwrap()
            .write_all(b"\x89PNG\x0D\x0A\x1A\x0A")
            .unwrap();
        assert!(reader.is_binary_file(&png_file, "fake.png").unwrap());

        // Test ZIP file signature (应该被 file-format 识别为二进制)
        let zip_file = temp_path.join("test.zip");
        File::create(&zip_file)
            .unwrap()
            .write_all(b"PK\x03\x04")
            .unwrap();
        assert!(reader.is_binary_file(&zip_file, "test.zip").unwrap());

        // Test empty file
        let empty_file = temp_path.join("empty.txt");
        File::create(&empty_file).unwrap().write_all(b"").unwrap();
        assert!(!reader.is_binary_file(&empty_file, "empty.txt").unwrap());

        // Test text file with binary extension (应该是文本，不是二进制)
        let text_with_bin_ext = temp_path.join("text.o");
        File::create(&text_with_bin_ext)
            .unwrap()
            .write_all(b"// This is actually C source code\nint main() { return 0; }")
            .unwrap();
        assert!(!reader.is_binary_file(&text_with_bin_ext, "text.o").unwrap());

        // Test actual binary file with more null bytes (确保备用检测逻辑工作)
        let more_binary_file = temp_path.join("more_binary.dat");
        let mut binary_content = Vec::new();
        for i in 0..200 {
            binary_content.push(if i % 10 == 0 { 0 } else { b'A' });
        }
        File::create(&more_binary_file)
            .unwrap()
            .write_all(&binary_content)
            .unwrap();
        assert!(
            reader
                .is_binary_file(&more_binary_file, "more_binary.dat")
                .unwrap()
        );
    }

    #[test]
    fn test_default_hidden_dir_exclusion() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path();

        // Create test files including hidden directories
        File::create(temp_path.join("main.rs"))
            .unwrap()
            .write_all(b"fn main() {}")
            .unwrap();
        File::create(temp_path.join("README.md"))
            .unwrap()
            .write_all(b"# Test")
            .unwrap();

        // Create hidden directory with files
        fs::create_dir(temp_path.join(".git")).unwrap();
        File::create(temp_path.join(".git/config"))
            .unwrap()
            .write_all(b"git config")
            .unwrap();
        File::create(temp_path.join(".git/HEAD"))
            .unwrap()
            .write_all(b"ref: main")
            .unwrap();

        // Create another hidden directory
        fs::create_dir(temp_path.join(".vscode")).unwrap();
        File::create(temp_path.join(".vscode/settings.json"))
            .unwrap()
            .write_all(b"{}")
            .unwrap();

        // Create nested hidden directory
        fs::create_dir(temp_path.join(".git/objects")).unwrap();
        File::create(temp_path.join(".git/objects/test"))
            .unwrap()
            .write_all(b"object data")
            .unwrap();

        let reader = ProjectReader::new();
        let result = reader.read_project(temp_path).unwrap();

        // Check that only non-hidden files are included
        let files = result.files;
        assert_eq!(files.len(), 2); // Only main.rs and README.md

        assert!(files.iter().any(|f| f.name == "main.rs"));
        assert!(files.iter().any(|f| f.name == "README.md"));

        // Check that hidden directory files are excluded
        assert!(!files.iter().any(|f| f.name.starts_with(".git/")));
        assert!(!files.iter().any(|f| f.name.starts_with(".vscode/")));
    }

    #[test]
    fn test_include_hidden_dirs_override() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path();

        // Create test files including hidden directories
        File::create(temp_path.join("main.rs"))
            .unwrap()
            .write_all(b"fn main() {}")
            .unwrap();

        // Create hidden directory with files
        fs::create_dir(temp_path.join(".git")).unwrap();
        File::create(temp_path.join(".git/config"))
            .unwrap()
            .write_all(b"git config")
            .unwrap();

        // Create config that includes hidden directories but still excludes by regex pattern
        let config = ProjectReadConfigBuilder::default()
            .include_hidden_dirs(true)
            .include_hidden_files(true)
            .build()
            .unwrap();

        let reader = ProjectReader::with_config(config);
        let result = reader.read_project(temp_path).unwrap();

        // Hidden directory files should still be excluded due to regex pattern
        let files = result.files;
        assert!(!files.iter().any(|f| f.name.starts_with(".git/")));
    }
}

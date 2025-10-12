use anyhow::{Context, Result};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tracing::{debug, info};
use walkdir::WalkDir;
use zip::{ZipWriter, write::FileOptions};

/// 项目压缩器
pub struct ProjectZipper;

/// 需要排除的目录名称列表
const EXCLUDED_DIRECTORIES: &[&str] = &[
    "node_modules",
    // 可以在这里添加更多需要排除的目录名称
];

impl ProjectZipper {
    /// 创建新的项目压缩器实例
    pub fn new() -> Self {
        Self
    }

    /// 压缩项目目录为 ZIP 文件
    ///
    /// # 参数
    /// * `project_path` - 项目目录的绝对路径
    /// * `output_path` - 输出的 ZIP 文件路径（可选，如果不提供则使用项目名生成）
    ///
    /// # 返回
    /// 返回生成的 ZIP 文件路径
    ///
    /// # 示例
    /// ```rust
    /// use nuwax_parser::project_op::ProjectZipper;
    ///
    /// let zipper = ProjectZipper::new();
    /// let zip_path = zipper.zip_project("/path/to/project", None)?;
    /// println!("项目已压缩到: {:?}", zip_path);
    /// ```
    pub fn zip_project<P: AsRef<Path>>(
        &self,
        project_path: P,
        output_path: Option<P>,
    ) -> Result<PathBuf> {
        let project_path = project_path.as_ref();

        // 验证项目目录是否存在
        if !project_path.exists() {
            return Err(anyhow::anyhow!("项目目录不存在: {:?}", project_path));
        }

        if !project_path.is_dir() {
            return Err(anyhow::anyhow!("路径不是目录: {:?}", project_path));
        }

        info!("开始压缩项目: {:?}", project_path);

        // 确定输出路径
        let output_path = output_path
            .map(|p| p.as_ref().to_path_buf())
            .unwrap_or_else(|| {
                let project_name = project_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("project");
                PathBuf::from(format!("{}.zip", project_name))
            });

        // 创建 ZIP 文件
        let zip_file =
            File::create(&output_path).context(format!("无法创建 ZIP 文件: {:?}", output_path))?;

        let mut zip = ZipWriter::new(zip_file);
        let options = FileOptions::<'_, ()>::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o755);

        // 遍历项目目录并添加文件到 ZIP
        for entry in WalkDir::new(project_path)
            .into_iter()
            .filter_entry(|e| {
                // 排除指定的目录
                !EXCLUDED_DIRECTORIES.iter().any(|excluded| {
                    e.path()
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(|name| name == *excluded)
                        .unwrap_or(false)
                })
            })
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            let name = path
                .strip_prefix(project_path)
                .map_err(|e| anyhow::anyhow!("路径前缀移除失败: {}", e))?;

            // 跳过目录本身（只添加文件）
            if path.is_dir() {
                debug!("跳过目录: {:?}", name);
                continue;
            }

            debug!("添加文件到 ZIP: {:?}", name);

            // 读取文件内容
            let mut file = File::open(path).context(format!("无法打开文件: {:?}", path))?;
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer)
                .context(format!("无法读取文件: {:?}", path))?;

            // 添加文件到 ZIP
            zip.start_file(name.to_string_lossy(), options)
                .context(format!("无法添加文件到 ZIP: {:?}", name))?;
            zip.write_all(&buffer)
                .context(format!("无法写入文件内容到 ZIP: {:?}", name))?;
        }

        // 完成 ZIP 文件
        zip.finish().context("无法完成 ZIP 文件创建")?;

        info!("项目压缩完成: {:?}", output_path);
        Ok(output_path)
    }

    /// 压缩项目目录到内存中的字节数组
    ///
    /// # 参数
    /// * `project_path` - 项目目录的绝对路径
    ///
    /// # 返回
    /// 返回 ZIP 文件的字节数组
    pub fn zip_project_to_bytes<P: AsRef<Path>>(&self, project_path: P) -> Result<Vec<u8>> {
        let project_path = project_path.as_ref();

        // 创建临时文件
        let temp_dir = tempfile::TempDir::new()?;
        let temp_zip_path = temp_dir.path().join("project.zip");

        // 压缩到临时文件
        let zip_path = self.zip_project(project_path, Some(&temp_zip_path))?;

        // 读取 ZIP 文件内容
        let mut zip_file = File::open(zip_path)?;
        let mut buffer = Vec::new();
        zip_file.read_to_end(&mut buffer)?;

        Ok(buffer)
    }
}

impl Default for ProjectZipper {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_zip_project() {
        // 创建测试项目结构
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path();

        // 创建测试文件
        fs::write(
            project_path.join("README.md"),
            "# Test Project\nThis is a test.",
        )
        .unwrap();
        fs::write(
            project_path.join("main.rs"),
            "fn main() { println!(\"Hello\"); }",
        )
        .unwrap();

        // 创建子目录和文件
        let src_dir = project_path.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("utils.rs"), "pub fn utils() {}").unwrap();

        // 创建排除的目录（应该被排除）
        for excluded_dir in EXCLUDED_DIRECTORIES {
            let excluded_path = project_path.join(excluded_dir);
            fs::create_dir_all(&excluded_path).unwrap();
            fs::write(
                excluded_path.join("test_file.txt"),
                format!("This file should be excluded from {}", excluded_dir),
            )
            .unwrap();
        }

        // 测试压缩
        let zipper = ProjectZipper::new();
        let zip_path = zipper.zip_project(project_path, None).unwrap();

        // 验证 ZIP 文件存在
        assert!(zip_path.exists());

        // 验证 ZIP 文件内容
        let zip_file = File::open(zip_path).unwrap();
        let archive = zip::ZipArchive::new(zip_file).unwrap();

        // 应该有 3 个文件（README.md, main.rs, src/utils.rs），不包括 node_modules
        assert_eq!(archive.len(), 3);

        // 验证文件名
        let names: Vec<String> = archive.file_names().map(|s| s.to_string()).collect();
        assert!(names.contains(&"README.md".to_string()));
        assert!(names.contains(&"main.rs".to_string()));
        assert!(names.contains(&"src/utils.rs".to_string()));

        // 验证排除的目录确实被排除了
        for excluded_dir in EXCLUDED_DIRECTORIES {
            assert!(!names.iter().any(|name| name.contains(excluded_dir)));
        }
    }
}

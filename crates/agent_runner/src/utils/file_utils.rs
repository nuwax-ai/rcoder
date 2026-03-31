//! 文件处理工具函数
//!
//! 提供文件读取、验证、转换等实用功能

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::warn;

use super::ContentBuilder;
use shared_types::{Attachment, AttachmentError, AttachmentSource};

/// 文件处理配置
pub struct FileConfig {
    /// 最大文件大小（字节）
    pub max_file_size: u64,
    /// 允许的文件扩展名
    pub allowed_extensions: Vec<String>,
    /// 临时文件目录
    pub temp_dir: PathBuf,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            max_file_size: 50 * 1024 * 1024, // 50MB
            allowed_extensions: vec![
                "txt", "md", "json", "xml", "html", "css", "js", "ts", // 文本文件
                "jpg", "jpeg", "png", "gif", "svg", "webp", // 图像文件
                "mp3", "wav", "ogg", "m4a", // 音频文件
                "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", // 文档文件
                "zip", "tar", "gz", "rar", "7z", // 压缩文件
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            temp_dir: std::env::temp_dir(),
        }
    }
}

/// 文件处理工具
pub struct FileUtils {
    config: FileConfig,
}

impl FileUtils {
    /// 创建新的文件处理工具实例
    pub fn new() -> Self {
        Self {
            config: FileConfig::default(),
        }
    }

    /// 使用自定义配置创建文件处理工具
    pub fn with_config(config: FileConfig) -> Self {
        Self { config }
    }

    /// 验证文件扩展名是否允许
    pub fn validate_extension(&self, filename: &str) -> Result<()> {
        let path = Path::new(filename);
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .ok_or_else(|| anyhow::anyhow!("文件没有扩展名"))?;

        if !self
            .config
            .allowed_extensions
            .contains(&extension.to_lowercase())
        {
            return Err(anyhow::anyhow!("不支持的文件扩展名: {}", extension));
        }

        Ok(())
    }

    /// 验证文件大小
    pub async fn validate_file_size(&self, file_path: &Path) -> Result<()> {
        let metadata = fs::metadata(file_path)
            .await
            .context("无法获取文件元数据")?;

        if metadata.len() > self.config.max_file_size {
            return Err(AttachmentError::FileSizeExceeded(metadata.len()).into());
        }

        Ok(())
    }

    /// 读取文件内容为 base64
    pub async fn read_file_as_base64(&self, file_path: &Path) -> Result<String> {
        self.validate_file_size(file_path).await?;

        let content = fs::read(file_path).await.context("读取文件失败")?;

        Ok(general_purpose::STANDARD.encode(content))
    }

    /// 读取文本文件
    pub async fn read_text_file(&self, file_path: &Path) -> Result<String> {
        self.validate_file_size(file_path).await?;

        let content = fs::read_to_string(file_path)
            .await
            .context("读取文本文件失败")?;

        Ok(content)
    }

    /// 从文件路径创建附件
    pub async fn create_attachment_from_file(
        &self,
        file_path: &Path,
        project_path: &Path,
        description: Option<String>,
    ) -> Result<Attachment> {
        // 验证文件扩展名
        let filename = file_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("无效的文件名"))?;

        self.validate_extension(filename)?;

        // 获取相对于项目路径的路径
        let relative_path = file_path
            .strip_prefix(project_path)
            .unwrap_or(file_path)
            .to_string_lossy()
            .to_string();

        // 获取 MIME 类型
        let mime_type = ContentBuilder::infer_mime_type_from_extension(filename);

        // 根据文件类型创建相应的附件
        let attachment = if mime_type.starts_with("image/") {
            Attachment::new_image(
                AttachmentSource::FilePath {
                    path: relative_path,
                },
                mime_type.to_string(),
            )
        } else if mime_type.starts_with("audio/") {
            Attachment::new_audio(
                AttachmentSource::FilePath {
                    path: relative_path,
                },
                mime_type.to_string(),
            )
        } else if mime_type.starts_with("text/") || mime_type == "application/json" {
            let mut attachment = Attachment::new_text(AttachmentSource::FilePath {
                path: relative_path,
            });
            if let Attachment::Text(ref mut text_attachment) = attachment {
                text_attachment.description = description;
            }
            attachment
        } else {
            Attachment::new_document(
                AttachmentSource::FilePath {
                    path: relative_path,
                },
                mime_type.to_string(),
            )
        };

        Ok(attachment)
    }

    /// 从 base64 数据创建附件
    pub fn create_attachment_from_base64(
        &self,
        data: String,
        mime_type: String,
        filename: Option<String>,
        description: Option<String>,
    ) -> Result<Attachment> {
        // 验证数据大小
        let decoded_size = general_purpose::STANDARD.decode(&data)?.len() as u64;
        if decoded_size > self.config.max_file_size {
            return Err(AttachmentError::FileSizeExceeded(decoded_size).into());
        }

        let source = AttachmentSource::Base64 {
            data,
            mime_type: mime_type.clone(),
        };

        let attachment = if mime_type.starts_with("image/") {
            let mut attachment = Attachment::new_image(source, mime_type);
            if let Attachment::Image(ref mut image_attachment) = attachment {
                image_attachment.filename = filename;
                image_attachment.description = description;
            }
            attachment
        } else if mime_type.starts_with("audio/") {
            let mut attachment = Attachment::new_audio(source, mime_type);
            if let Attachment::Audio(ref mut audio_attachment) = attachment {
                audio_attachment.filename = filename;
                audio_attachment.description = description;
            }
            attachment
        } else if mime_type.starts_with("text/") || mime_type == "application/json" {
            let mut attachment = Attachment::new_text(source);
            if let Attachment::Text(ref mut text_attachment) = attachment {
                text_attachment.filename = filename;
                text_attachment.description = description;
            }
            attachment
        } else {
            let mut attachment = Attachment::new_document(source, mime_type);
            if let Attachment::Document(ref mut doc_attachment) = attachment {
                doc_attachment.filename = filename;
                doc_attachment.description = description;
                doc_attachment.size = Some(decoded_size);
            }
            attachment
        };

        Ok(attachment)
    }

    /// 从 URL 创建附件
    pub async fn create_attachment_from_url(
        &self,
        url: &str,
        filename: Option<String>,
        description: Option<String>,
    ) -> Result<Attachment> {
        // 获取 URL 头信息以确定文件类型和大小
        let client = reqwest::Client::new();
        let response = client.head(url).send().await?;

        // 检查文件大小
        if let Some(content_length) = response.headers().get(reqwest::header::CONTENT_LENGTH) {
            let length: u64 = content_length.to_str()?.parse()?;
            if length > self.config.max_file_size {
                return Err(AttachmentError::FileSizeExceeded(length).into());
            }
        }

        // 获取 MIME 类型
        let mime_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream");

        let source = AttachmentSource::Url {
            url: url.to_string(),
        };

        let attachment = if mime_type.starts_with("image/") {
            let mut attachment = Attachment::new_image(source, mime_type.to_string());
            if let Attachment::Image(ref mut image_attachment) = attachment {
                image_attachment.filename = filename;
                image_attachment.description = description;
            }
            attachment
        } else if mime_type.starts_with("audio/") {
            let mut attachment = Attachment::new_audio(source, mime_type.to_string());
            if let Attachment::Audio(ref mut audio_attachment) = attachment {
                audio_attachment.filename = filename;
                audio_attachment.description = description;
            }
            attachment
        } else if mime_type.starts_with("text/") || mime_type == "application/json" {
            let mut attachment = Attachment::new_text(source);
            if let Attachment::Text(ref mut text_attachment) = attachment {
                text_attachment.filename = filename;
                text_attachment.description = description;
            }
            attachment
        } else {
            let mut attachment = Attachment::new_document(source, mime_type.to_string());
            if let Attachment::Document(ref mut doc_attachment) = attachment {
                doc_attachment.filename = filename;
                doc_attachment.description = description;
            }
            attachment
        };

        Ok(attachment)
    }

    /// 批量处理文件创建附件
    pub async fn create_attachments_from_files(
        &self,
        file_paths: &[PathBuf],
        project_path: &Path,
    ) -> Result<Vec<Attachment>> {
        let mut attachments = Vec::new();

        for file_path in file_paths {
            match self
                .create_attachment_from_file(file_path, project_path, None)
                .await
            {
                Ok(attachment) => attachments.push(attachment),
                Err(e) => {
                    warn!(
                        "Skipping file {:?}; failed to create attachment: {}",
                        file_path, e
                    );
                }
            }
        }

        Ok(attachments)
    }

    /// 清理临时文件
    pub async fn cleanup_temp_files(&self, temp_files: &[PathBuf]) -> Result<()> {
        for temp_file in temp_files {
            if temp_file.exists() {
                fs::remove_file(temp_file)
                    .await
                    .with_context(|| format!("删除临时文件失败: {:?}", temp_file))?;
            }
        }
        Ok(())
    }
}

impl Default for FileUtils {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_validate_extension() {
        let file_utils = FileUtils::new();

        // 测试允许的扩展名
        assert!(file_utils.validate_extension("test.txt").is_ok());
        assert!(file_utils.validate_extension("image.jpg").is_ok());
        assert!(file_utils.validate_extension("audio.mp3").is_ok());

        // 测试不允许的扩展名
        assert!(file_utils.validate_extension("test.exe").is_err());
        assert!(file_utils.validate_extension("test").is_err());
    }

    #[tokio::test]
    async fn test_read_file_as_base64() {
        let file_utils = FileUtils::new();

        // 创建临时文件
        let mut temp_file = NamedTempFile::new().unwrap();
        writeln!(temp_file, "Hello, World!").unwrap();
        let temp_path = temp_file.path();

        let base64_content = file_utils.read_file_as_base64(temp_path).await.unwrap();

        // 验证 base64 编码是否正确
        let decoded = general_purpose::STANDARD.decode(base64_content).unwrap();
        let decoded_text = String::from_utf8(decoded).unwrap();
        assert_eq!(decoded_text.trim(), "Hello, World!");
    }
}

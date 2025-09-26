//! 附件数据模型
//!
//! 定义支持多种媒体类型的附件结构，用于扩展聊天功能

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// 附件数据源类型
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "source_type", content = "data")]
pub enum AttachmentSource {
    /// 文件路径，相对于项目目录
    FilePath { path: String },
    /// Base64 编码的数据
    Base64 { data: String, mime_type: String },
    /// URL 链接
    Url { url: String },
}

/// 文本附件
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TextAttachment {
    /// 附件唯一标识
    pub id: String,
    /// 数据源
    pub source: AttachmentSource,
    /// 可选的文件名
    pub filename: Option<String>,
    /// 可选的描述
    pub description: Option<String>,
}

/// 图像附件
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ImageAttachment {
    /// 附件唯一标识
    pub id: String,
    /// 数据源
    pub source: AttachmentSource,
    /// MIME 类型 (如: image/jpeg, image/png)
    pub mime_type: String,
    /// 可选的文件名
    pub filename: Option<String>,
    /// 可选的描述
    pub description: Option<String>,
    /// 可选的尺寸信息
    pub dimensions: Option<ImageDimensions>,
}

/// 图像尺寸信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ImageDimensions {
    pub width: u32,
    pub height: u32,
}

/// 音频附件
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AudioAttachment {
    /// 附件唯一标识
    pub id: String,
    /// 数据源
    pub source: AttachmentSource,
    /// MIME 类型 (如: audio/mp3, audio/wav)
    pub mime_type: String,
    /// 可选的文件名
    pub filename: Option<String>,
    /// 可选的描述
    pub description: Option<String>,
    /// 可选的时长（秒）
    pub duration: Option<f64>,
}

/// 文档附件
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DocumentAttachment {
    /// 附件唯一标识
    pub id: String,
    /// 数据源
    pub source: AttachmentSource,
    /// MIME 类型 (如: application/pdf, text/plain)
    pub mime_type: String,
    /// 可选的文件名
    pub filename: Option<String>,
    /// 可选的描述
    pub description: Option<String>,
    /// 可选的文件大小（字节）
    pub size: Option<u64>,
}

/// 附件枚举 - 支持多种媒体类型
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", content = "content")]
pub enum Attachment {
    /// 文本附件
    Text(TextAttachment),
    /// 图像附件
    Image(ImageAttachment),
    /// 音频附件
    Audio(AudioAttachment),
    /// 文档附件
    Document(DocumentAttachment),
}

impl Attachment {
    /// 创建新的文本附件
    pub fn new_text(source: AttachmentSource) -> Self {
        Self::Text(TextAttachment {
            id: Uuid::new_v4().to_string(),
            source,
            filename: None,
            description: None,
        })
    }

    /// 创建新的图像附件
    pub fn new_image(source: AttachmentSource, mime_type: String) -> Self {
        Self::Image(ImageAttachment {
            id: Uuid::new_v4().to_string(),
            source,
            mime_type,
            filename: None,
            description: None,
            dimensions: None,
        })
    }

    /// 创建新的音频附件
    pub fn new_audio(source: AttachmentSource, mime_type: String) -> Self {
        Self::Audio(AudioAttachment {
            id: Uuid::new_v4().to_string(),
            source,
            mime_type,
            filename: None,
            description: None,
            duration: None,
        })
    }

    /// 创建新的文档附件
    pub fn new_document(source: AttachmentSource, mime_type: String) -> Self {
        Self::Document(DocumentAttachment {
            id: Uuid::new_v4().to_string(),
            source,
            mime_type,
            filename: None,
            description: None,
            size: None,
        })
    }

    /// 获取附件 ID
    pub fn id(&self) -> &str {
        match self {
            Attachment::Text(text) => &text.id,
            Attachment::Image(image) => &image.id,
            Attachment::Audio(audio) => &audio.id,
            Attachment::Document(doc) => &doc.id,
        }
    }

    /// 获取附件的 MIME 类型
    pub fn mime_type(&self) -> Option<&str> {
        match self {
            Attachment::Text(_) => Some("text/plain"),
            Attachment::Image(image) => Some(&image.mime_type),
            Attachment::Audio(audio) => Some(&audio.mime_type),
            Attachment::Document(doc) => Some(&doc.mime_type),
        }
    }

    /// 获取附件数据源
    pub fn source(&self) -> &AttachmentSource {
        match self {
            Attachment::Text(text) => &text.source,
            Attachment::Image(image) => &image.source,
            Attachment::Audio(audio) => &audio.source,
            Attachment::Document(doc) => &doc.source,
        }
    }
}

/// 附件处理错误
#[derive(Debug, thiserror::Error)]
pub enum AttachmentError {
    #[error("文件读取失败: {0}")]
    FileReadError(String),
    #[error("不支持的文件类型: {0}")]
    UnsupportedFileType(String),
    #[error("Base64 解码失败: {0}")]
    Base64DecodeError(String),
    #[error("文件大小超过限制: {0} bytes")]
    FileSizeExceeded(u64),
    #[error("URL 访问失败: {0}")]
    UrlAccessError(String),
}

impl From<std::io::Error> for AttachmentError {
    fn from(err: std::io::Error) -> Self {
        AttachmentError::FileReadError(err.to_string())
    }
}

impl From<base64::DecodeError> for AttachmentError {
    fn from(err: base64::DecodeError) -> Self {
        AttachmentError::Base64DecodeError(err.to_string())
    }
}

impl From<reqwest::Error> for AttachmentError {
    fn from(err: reqwest::Error) -> Self {
        AttachmentError::UrlAccessError(err.to_string())
    }
}
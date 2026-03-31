//! Content Builder 工具
//!
//! 将 Attachment 转换为 ACP 协议的 ContentBlock

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose};
use sacp::schema::{
    AudioContent, BlobResourceContents, ContentBlock, EmbeddedResource, EmbeddedResourceResource,
    ImageContent, ResourceLink, TextContent, TextResourceContents,
};
use std::path::Path;

use crate::model::{Attachment, AttachmentSource};

/// Content Builder - 将附件转换为 ACP ContentBlock
pub struct ContentBuilder;

impl ContentBuilder {
    /// 将单个附件转换为 ContentBlock
    pub async fn attachment_to_content_block(
        attachment: &Attachment,
        project_path: &std::path::Path,
    ) -> Result<Option<ContentBlock>> {
        match attachment {
            Attachment::Text(text_attachment) => {
                Self::text_to_content_block(text_attachment, project_path).await
            }
            Attachment::Image(image_attachment) => {
                Self::image_to_content_block(image_attachment, project_path).await
            }
            Attachment::Audio(audio_attachment) => {
                Self::audio_to_content_block(audio_attachment, project_path).await
            }
            Attachment::Document(document_attachment) => {
                Self::document_to_content_block(document_attachment, project_path).await
            }
        }
    }

    /// 将多个附件转换为 ContentBlock 列表
    /// 文件不存在或无法读取的附件会被静默忽略
    pub async fn attachments_to_content_blocks(
        attachments: &[Attachment],
        project_path: &std::path::Path,
    ) -> Result<Vec<ContentBlock>> {
        let mut content_blocks = Vec::new();

        for attachment in attachments {
            match Self::attachment_to_content_block(attachment, project_path).await {
                Ok(Some(content_block)) => {
                    content_blocks.push(content_block);
                }
                Ok(None) => {
                    // 文件不存在或无法读取，静默忽略
                    tracing::warn!(
                        "Attachment could not be loaded and was ignored: attachment_id={}",
                        attachment.id()
                    );
                }
                Err(e) => {
                    // 其他错误（如网络错误），记录警告但继续处理
                    tracing::warn!(
                        "⚠️ Attachment conversion failed and was ignored: attachment_id={}, error={:?}",
                        attachment.id(),
                        e
                    );
                }
            }
        }

        Ok(content_blocks)
    }

    /// 文本附件转换为 ContentBlock
    /// 如果文件不存在或是压缩文件，返回 Ok(None)
    async fn text_to_content_block(
        text_attachment: &crate::model::TextAttachment,
        project_path: &std::path::Path,
    ) -> Result<Option<ContentBlock>> {
        let text_content = match &text_attachment.source {
            AttachmentSource::FilePath { path } => {
                let file_path = project_path.join(path);

                // 检查文件是否存在
                if !file_path.exists() {
                    tracing::warn!("Attachment file not found, ignored: {:?}", file_path);
                    return Ok(None);
                }

                // 检查是否为二进制文件（常见压缩格式）
                if let Some(ext) = file_path.extension() {
                    let ext_str = ext.to_string_lossy().to_lowercase();
                    if matches!(
                        ext_str.as_str(),
                        "gz" | "zip" | "tar" | "bz2" | "xz" | "7z" | "rar"
                    ) {
                        tracing::warn!(
                            "⚠️ Compressed files are not supported as Text attachments, ignored: {:?} (extension: {})",
                            file_path,
                            ext_str
                        );
                        return Ok(None);
                    }
                }

                // 尝试读取文本文件
                let text = match tokio::fs::read_to_string(&file_path).await {
                    Ok(text) => text,
                    Err(e) => {
                        tracing::warn!(
                            "⚠️ Failed to read text file, ignored: {:?}, error: {} (may be binary or not UTF-8 encoded)",
                            file_path,
                            e
                        );
                        return Ok(None);
                    }
                };

                TextContent::new(text)
            }
            AttachmentSource::Base64 { data, mime_type: _ } => {
                let text = String::from_utf8(general_purpose::STANDARD.decode(data)?)?;

                TextContent::new(text)
            }
            AttachmentSource::Url { url } => {
                let client = reqwest::Client::new();
                let response = client.get(url).send().await?;
                let text = response.text().await?;

                TextContent::new(text)
            }
        };

        Ok(Some(ContentBlock::Text(text_content)))
    }

    /// 图像附件转换为 ContentBlock
    /// 如果文件不存在，返回 Ok(None)
    async fn image_to_content_block(
        image_attachment: &crate::model::ImageAttachment,
        project_path: &std::path::Path,
    ) -> Result<Option<ContentBlock>> {
        let (data, uri) = match &image_attachment.source {
            AttachmentSource::FilePath { path } => {
                let file_path = project_path.join(path);

                // 检查文件是否存在
                if !file_path.exists() {
                    tracing::warn!("Image file not found, ignored: {:?}", file_path);
                    return Ok(None);
                }

                // 尝试读取文件
                let data = match tokio::fs::read(&file_path).await {
                    Ok(data) => data,
                    Err(e) => {
                        tracing::warn!(
                            "Failed to read image file, ignored: {:?}, error: {}",
                            file_path,
                            e
                        );
                        return Ok(None);
                    }
                };
                let base64_data = general_purpose::STANDARD.encode(data);
                let uri = file_path.to_string_lossy().to_string();
                (base64_data, Some(uri))
            }
            AttachmentSource::Base64 { data, .. } => (data.clone(), None),
            AttachmentSource::Url { url } => {
                let client = reqwest::Client::new();
                let response = client.get(url).send().await?;
                let data = response.bytes().await?;
                let base64_data = general_purpose::STANDARD.encode(data);
                (base64_data, Some(url.clone()))
            }
        };

        let mut image_content = ImageContent::new(data, image_attachment.mime_type.clone());
        if let Some(u) = uri {
            image_content = image_content.uri(u);
        }
        Ok(Some(ContentBlock::Image(image_content)))
    }

    /// 音频附件转换为 ContentBlock
    /// 如果文件不存在，返回 Ok(None)
    async fn audio_to_content_block(
        audio_attachment: &crate::model::AudioAttachment,
        project_path: &std::path::Path,
    ) -> Result<Option<ContentBlock>> {
        let data = match &audio_attachment.source {
            AttachmentSource::FilePath { path } => {
                let file_path = project_path.join(path);

                // 检查文件是否存在
                if !file_path.exists() {
                    tracing::warn!("Audio file not found, ignored: {:?}", file_path);
                    return Ok(None);
                }

                // 尝试读取文件
                let data = match tokio::fs::read(&file_path).await {
                    Ok(data) => data,
                    Err(e) => {
                        tracing::warn!(
                            "Failed to read audio file, ignored: {:?}, error: {}",
                            file_path,
                            e
                        );
                        return Ok(None);
                    }
                };
                general_purpose::STANDARD.encode(data)
            }
            AttachmentSource::Base64 { data, .. } => data.clone(),
            AttachmentSource::Url { url } => {
                let client = reqwest::Client::new();
                let response = client.get(url).send().await?;
                let data = response.bytes().await?;
                general_purpose::STANDARD.encode(data)
            }
        };

        Ok(Some(ContentBlock::Audio(AudioContent::new(
            data,
            audio_attachment.mime_type.clone(),
        ))))
    }

    /// 文档附件转换为 ContentBlock
    /// 如果文件不存在，返回 Ok(None)
    async fn document_to_content_block(
        document_attachment: &crate::model::DocumentAttachment,
        project_path: &std::path::Path,
    ) -> Result<Option<ContentBlock>> {
        match &document_attachment.source {
            AttachmentSource::FilePath { path } => {
                let file_path = project_path.join(path);

                // 检查文件是否存在
                if !file_path.exists() {
                    tracing::warn!("Document file not found, ignored: {:?}", file_path);
                    return Ok(None);
                }

                let uri = file_path.to_string_lossy().to_string();

                // 尝试读取为文本，如果失败则作为二进制处理
                if document_attachment.mime_type.starts_with("text/") {
                    match tokio::fs::read_to_string(&file_path).await {
                        Ok(text) => {
                            let mut text_contents = TextResourceContents::new(text, uri);
                            if let Some(mime_type) = Some(document_attachment.mime_type.clone()) {
                                text_contents = text_contents.mime_type(mime_type);
                            }
                            Ok(Some(ContentBlock::Resource(EmbeddedResource::new(
                                EmbeddedResourceResource::TextResourceContents(text_contents),
                            ))))
                        }
                        Err(_) => {
                            // 文本读取失败，尝试作为二进制处理
                            Self::handle_binary_document(
                                &file_path,
                                &document_attachment.mime_type,
                                &uri,
                            )
                            .await
                        }
                    }
                } else {
                    Self::handle_binary_document(&file_path, &document_attachment.mime_type, &uri)
                        .await
                }
            }
            AttachmentSource::Base64 { data, mime_type } => {
                let uri = format!("base64://{}", document_attachment.id);

                if mime_type.starts_with("text/") {
                    match String::from_utf8(general_purpose::STANDARD.decode(data)?) {
                        Ok(text) => {
                            let mut text_contents = TextResourceContents::new(text, uri);
                            text_contents = text_contents.mime_type(mime_type.clone());
                            Ok(Some(ContentBlock::Resource(EmbeddedResource::new(
                                EmbeddedResourceResource::TextResourceContents(text_contents),
                            ))))
                        }
                        Err(_) => {
                            // 文本解码失败，作为二进制处理
                            let mut blob_contents = BlobResourceContents::new(data.clone(), uri);
                            blob_contents = blob_contents.mime_type(mime_type.clone());
                            Ok(Some(ContentBlock::Resource(EmbeddedResource::new(
                                EmbeddedResourceResource::BlobResourceContents(blob_contents),
                            ))))
                        }
                    }
                } else {
                    let mut blob_contents = BlobResourceContents::new(data.clone(), uri);
                    blob_contents = blob_contents.mime_type(mime_type.clone());
                    Ok(Some(ContentBlock::Resource(EmbeddedResource::new(
                        EmbeddedResourceResource::BlobResourceContents(blob_contents),
                    ))))
                }
            }
            AttachmentSource::Url { url } => {
                // URL 资源作为 ResourceLink 处理
                let name = document_attachment
                    .filename
                    .clone()
                    .unwrap_or_else(|| "document".to_string());
                let mut resource_link = ResourceLink::new(name, url.clone());
                resource_link = resource_link.mime_type(document_attachment.mime_type.clone());

                // 只有当值存在时才设置
                if let Some(description) = &document_attachment.description {
                    resource_link = resource_link.description(description.clone());
                }
                if let Some(size) = document_attachment.size {
                    resource_link = resource_link.size(size as i64);
                }
                if let Some(filename) = &document_attachment.filename {
                    resource_link = resource_link.title(filename.clone());
                }
                Ok(Some(ContentBlock::ResourceLink(resource_link)))
            }
        }
    }

    /// 处理二进制文档
    /// 如果文件无法读取，返回 Ok(None)
    async fn handle_binary_document(
        file_path: &Path,
        mime_type: &str,
        uri: &str,
    ) -> Result<Option<ContentBlock>> {
        let data = match tokio::fs::read(file_path).await {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!(
                    "⚠️ 无法读取二进制文档，已忽略: {:?}，错误: {}",
                    file_path,
                    e
                );
                return Ok(None);
            }
        };
        let blob = general_purpose::STANDARD.encode(data);

        let mut blob_contents = BlobResourceContents::new(blob, uri.to_string());
        blob_contents = blob_contents.mime_type(mime_type.to_string());
        Ok(Some(ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::BlobResourceContents(blob_contents),
        ))))
    }

    /// 从文件扩展名推断 MIME 类型
    pub fn infer_mime_type_from_extension(filename: &str) -> &'static str {
        let path = std::path::Path::new(filename);
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("txt") => "text/plain",
            Some("md") => "text/markdown",
            Some("json") => "application/json",
            Some("xml") => "application/xml",
            Some("html") => "text/html",
            Some("css") => "text/css",
            Some("js") => "application/javascript",
            Some("ts") => "application/typescript",
            Some("pdf") => "application/pdf",
            Some("jpg") | Some("jpeg") => "image/jpeg",
            Some("png") => "image/png",
            Some("gif") => "image/gif",
            Some("svg") => "image/svg+xml",
            Some("webp") => "image/webp",
            Some("mp3") => "audio/mpeg",
            Some("wav") => "audio/wav",
            Some("ogg") => "audio/ogg",
            Some("m4a") => "audio/mp4",
            Some("mp4") => "video/mp4",
            Some("webm") => "video/webm",
            Some("zip") => "application/zip",
            Some("tar") => "application/x-tar",
            Some("gz") => "application/gzip",
            Some("rar") => "application/x-rar-compressed",
            Some("7z") => "application/x-7z-compressed",
            Some("doc") => "application/msword",
            Some("docx") => {
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            }
            Some("xls") => "application/vnd.ms-excel",
            Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            Some("ppt") => "application/vnd.ms-powerpoint",
            Some("pptx") => {
                "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            }
            _ => "application/octet-stream",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_mime_type() {
        assert_eq!(
            ContentBuilder::infer_mime_type_from_extension("test.txt"),
            "text/plain"
        );
        assert_eq!(
            ContentBuilder::infer_mime_type_from_extension("image.jpg"),
            "image/jpeg"
        );
        assert_eq!(
            ContentBuilder::infer_mime_type_from_extension("audio.mp3"),
            "audio/mpeg"
        );
        assert_eq!(
            ContentBuilder::infer_mime_type_from_extension("document.pdf"),
            "application/pdf"
        );
        assert_eq!(
            ContentBuilder::infer_mime_type_from_extension("unknown.xyz"),
            "application/octet-stream"
        );
    }
}

//! Content Builder 工具
//!
//! 将 Attachment 转换为 ACP 协议的 ContentBlock

use agent_client_protocol::{
    AudioContent, BlobResourceContents, ContentBlock, EmbeddedResource, EmbeddedResourceResource,
    ImageContent, ResourceLink, TextContent, TextResourceContents,
};
use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose};
use std::path::Path;

use crate::model::{Attachment, AttachmentSource};

/// Content Builder - 将附件转换为 ACP ContentBlock
pub struct ContentBuilder;

impl ContentBuilder {
    /// 将单个附件转换为 ContentBlock
    pub async fn attachment_to_content_block(
        attachment: &Attachment,
        project_path: &std::path::Path,
    ) -> Result<ContentBlock> {
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
    pub async fn attachments_to_content_blocks(
        attachments: &[Attachment],
        project_path: &std::path::Path,
    ) -> Result<Vec<ContentBlock>> {
        let mut content_blocks = Vec::new();

        for attachment in attachments {
            let content_block = Self::attachment_to_content_block(attachment, project_path)
                .await
                .with_context(|| {
                    format!(
                        "Failed to convert attachment {} to content block",
                        attachment.id()
                    )
                })?;
            content_blocks.push(content_block);
        }

        Ok(content_blocks)
    }

    /// 文本附件转换为 ContentBlock
    async fn text_to_content_block(
        text_attachment: &crate::model::TextAttachment,
        project_path: &std::path::Path,
    ) -> Result<ContentBlock> {
        let text_content = match &text_attachment.source {
            AttachmentSource::FilePath { path } => {
                let file_path = project_path.join(path);
                let text = tokio::fs::read_to_string(&file_path)
                    .await
                    .with_context(|| format!("Failed to read text file: {:?}", file_path))?;

                TextContent {
                    text,
                    annotations: None,
                    meta: None,
                }
            }
            AttachmentSource::Base64 { data, mime_type: _ } => {
                let text = String::from_utf8(general_purpose::STANDARD.decode(data)?)?;

                TextContent {
                    text,
                    annotations: None,
                    meta: None,
                }
            }
            AttachmentSource::Url { url } => {
                let client = reqwest::Client::new();
                let response = client.get(url).send().await?;
                let text = response.text().await?;

                TextContent {
                    text,
                    annotations: None,
                    meta: None,
                }
            }
        };

        Ok(ContentBlock::Text(text_content))
    }

    /// 图像附件转换为 ContentBlock
    async fn image_to_content_block(
        image_attachment: &crate::model::ImageAttachment,
        project_path: &std::path::Path,
    ) -> Result<ContentBlock> {
        let (data, uri) = match &image_attachment.source {
            AttachmentSource::FilePath { path } => {
                let file_path = project_path.join(path);
                let data = tokio::fs::read(&file_path)
                    .await
                    .with_context(|| format!("Failed to read image file: {:?}", file_path))?;
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

        Ok(ContentBlock::Image(ImageContent {
            data,
            mime_type: image_attachment.mime_type.clone(),
            uri,
            annotations: None,
            meta: None,
        }))
    }

    /// 音频附件转换为 ContentBlock
    async fn audio_to_content_block(
        audio_attachment: &crate::model::AudioAttachment,
        project_path: &std::path::Path,
    ) -> Result<ContentBlock> {
        let data = match &audio_attachment.source {
            AttachmentSource::FilePath { path } => {
                let file_path = project_path.join(path);
                let data = tokio::fs::read(&file_path)
                    .await
                    .with_context(|| format!("Failed to read audio file: {:?}", file_path))?;
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

        Ok(ContentBlock::Audio(AudioContent {
            data,
            mime_type: audio_attachment.mime_type.clone(),
            annotations: None,
            meta: None,
        }))
    }

    /// 文档附件转换为 ContentBlock
    async fn document_to_content_block(
        document_attachment: &crate::model::DocumentAttachment,
        project_path: &std::path::Path,
    ) -> Result<ContentBlock> {
        match &document_attachment.source {
            AttachmentSource::FilePath { path } => {
                let file_path = project_path.join(path);
                let uri = file_path.to_string_lossy().to_string();

                // 尝试读取为文本，如果失败则作为二进制处理
                if document_attachment.mime_type.starts_with("text/") {
                    match tokio::fs::read_to_string(&file_path).await {
                        Ok(text) => Ok(ContentBlock::Resource(EmbeddedResource {
                            resource: EmbeddedResourceResource::TextResourceContents(
                                TextResourceContents {
                                    mime_type: Some(document_attachment.mime_type.clone()),
                                    text,
                                    uri,
                                    meta: None,
                                },
                            ),
                            annotations: None,
                            meta: None,
                        })),
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
                        Ok(text) => Ok(ContentBlock::Resource(EmbeddedResource {
                            resource: EmbeddedResourceResource::TextResourceContents(
                                TextResourceContents {
                                    mime_type: Some(mime_type.clone()),
                                    text,
                                    uri,
                                    meta: None,
                                },
                            ),
                            annotations: None,
                            meta: None,
                        })),
                        Err(_) => {
                            // 文本解码失败，作为二进制处理
                            Ok(ContentBlock::Resource(EmbeddedResource {
                                resource: EmbeddedResourceResource::BlobResourceContents(
                                    BlobResourceContents {
                                        blob: data.clone(),
                                        mime_type: Some(mime_type.clone()),
                                        uri,
                                        meta: None,
                                    },
                                ),
                                annotations: None,
                                meta: None,
                            }))
                        }
                    }
                } else {
                    Ok(ContentBlock::Resource(EmbeddedResource {
                        resource: EmbeddedResourceResource::BlobResourceContents(
                            BlobResourceContents {
                                blob: data.clone(),
                                mime_type: Some(mime_type.clone()),
                                uri,
                                meta: None,
                            },
                        ),
                        annotations: None,
                        meta: None,
                    }))
                }
            }
            AttachmentSource::Url { url } => {
                // URL 资源作为 ResourceLink 处理
                Ok(ContentBlock::ResourceLink(ResourceLink {
                    name: document_attachment
                        .filename
                        .clone()
                        .unwrap_or_else(|| "document".to_string()),
                    uri: url.clone(),
                    mime_type: Some(document_attachment.mime_type.clone()),
                    description: document_attachment.description.clone(),
                    size: document_attachment.size.map(|s| s as i64),
                    title: document_attachment.filename.clone(),
                    annotations: None,
                    meta: None,
                }))
            }
        }
    }

    /// 处理二进制文档
    async fn handle_binary_document(
        file_path: &Path,
        mime_type: &str,
        uri: &str,
    ) -> Result<ContentBlock> {
        let data = tokio::fs::read(file_path)
            .await
            .with_context(|| format!("Failed to read binary document: {:?}", file_path))?;
        let blob = general_purpose::STANDARD.encode(data);

        Ok(ContentBlock::Resource(EmbeddedResource {
            resource: EmbeddedResourceResource::BlobResourceContents(BlobResourceContents {
                blob,
                mime_type: Some(mime_type.to_string()),
                uri: uri.to_string(),
                meta: None,
            }),
            annotations: None,
            meta: None,
        }))
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

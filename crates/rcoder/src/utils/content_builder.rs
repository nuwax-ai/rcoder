//! 内容构建工具
//!
//! 用于构建和处理 AI 代理的内容，包括附件转换等

use anyhow::Result;
use std::path::Path;
use crate::model::Attachment;
use agent_client_protocol::{ContentBlock, TextContent, ImageContent};

/// 内容构建器
pub struct ContentBuilder;

impl ContentBuilder {
    /// 将附件转换为内容块
    pub async fn attachments_to_content_blocks(
        attachments: &[Attachment],
        project_path: &Path,
    ) -> Result<Vec<ContentBlock>> {
        let mut content_blocks = Vec::new();

        for attachment in attachments {
            let content_block = match attachment.source() {
                crate::model::AttachmentSource::FilePath { path } => {
                    // 根据文件扩展名推断内容类型
                    let content = std::fs::read_to_string(&path)
                        .map_err(|e| anyhow::anyhow!("读取文件失败: {}", e))?;

                    ContentBlock::Text(TextContent {
                        text: content,
                        annotations: None,
                        meta: None,
                    })
                }
                crate::model::AttachmentSource::Base64 { data, mime_type } => {
                    // 根据MIME类型处理
                    if mime_type.starts_with("image/") {
                        ContentBlock::Image(ImageContent {
                            data: data.clone(),
                            mime_type: mime_type.clone(),
                            uri: None,
                            meta: None,
                            annotations: None,
                        })
                    } else {
                        // 其他类型统一作为文本处理
                        ContentBlock::Text(TextContent {
                            text: format!("[{}] {}", mime_type, "Base64 encoded data"),
                            annotations: None,
                            meta: None,
                        })
                    }
                }
                crate::model::AttachmentSource::Url { url } => {
                    // URL作为文本处理
                    ContentBlock::Text(TextContent {
                        text: format!("URL: {}", url),
                        annotations: None,
                        meta: None,
                    })
                }
            };

            content_blocks.push(content_block);
        }

        Ok(content_blocks)
    }
}
//! 多媒体聊天示例
//!
//! 展示如何使用扩展的聊天接口发送文本和多媒体内容

use rcoder::{AgentType, Attachment, AttachmentSource, ContentBuilder, FileUtils};
use serde_json::json;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志
    tracing_subscriber::fmt::init();

    println!("🚀 RCoder 多媒体聊天示例");
    println!("{}", "=".repeat(40));

    // 示例 1: 创建各种类型的附件
    println!("\n📝 示例 1: 创建各种类型的附件");

    // 文本附件
    let text_attachment = Attachment::new_text(AttachmentSource::FilePath {
        path: "docs/readme.md".to_string(),
    });
    println!("文本附件: {}", json!(text_attachment).to_string());

    // 图片附件
    let image_attachment = Attachment::new_image(
        AttachmentSource::Base64 {
            data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==".to_string(),
            mime_type: "image/png".to_string(),
        },
        "image/png".to_string(),
    );
    println!("图片附件: {}", json!(image_attachment).to_string());

    // 音频附件
    let audio_attachment = Attachment::new_audio(
        AttachmentSource::Url {
            url: "https://example.com/audio.mp3".to_string(),
        },
        "audio/mp3".to_string(),
    );
    println!("音频附件: {}", json!(audio_attachment).to_string());

    // 文档附件
    let doc_attachment = Attachment::new_document(
        AttachmentSource::FilePath {
            path: "docs/manual.pdf".to_string(),
        },
        "application/pdf".to_string(),
    );
    println!("文档附件: {}", json!(doc_attachment).to_string());

    // 示例 2: 多附件组合
    println!("\n🎯 示例 2: 多附件组合");
    let mixed_attachments = vec![
        text_attachment,
        image_attachment,
        audio_attachment,
        doc_attachment,
    ];
    println!("多附件组合: {}", json!(mixed_attachments).to_string());

    // 示例 3: 使用文件工具
    println!("\n🛠️ 示例 3: 使用文件工具");
    let file_utils = FileUtils::new();

    // 模拟创建测试文件
    let project_path = PathBuf::from("./test_project");
    tokio::fs::create_dir_all(&project_path).await?;

    // 创建测试文件
    let test_file = project_path.join("example.txt");
    tokio::fs::write(&test_file, "这是一个测试文件的内容\n用于演示文件处理功能").await?;

    // 从文件创建附件
    let file_attachment = file_utils
        .create_attachment_from_file(&test_file, &project_path, Some("示例文件".to_string()))
        .await?;

    println!("从文件创建的附件: {}", json!(file_attachment).to_string());

    // 清理测试文件
    tokio::fs::remove_dir_all(&project_path).await?;

    println!("\n✅ 所有示例执行完成！");
    println!("\n💡 多媒体聊天功能说明:");
    println!("1. 文本附件：支持文件路径、Base64 数据、URL 链接");
    println!("2. 图像附件：支持常见图像格式，自动 MIME 类型检测");
    println!("3. 音频附件：支持 MP3、WAV、OGG 等音频格式");
    println!("4. 文档附件：支持 PDF、Word、文本等文档格式");
    println!("5. 多附件组合：可在单个请求中包含多个不同类型的附件");
    println!("6. 文件工具：提供文件验证、大小限制、类型检测等功能");
    println!("7. ACP 兼容：完全兼容 Agent Client Protocol 标准");

    Ok(())
}

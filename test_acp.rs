// Simple test to verify ACP protocol compilation
use agent_client_protocol::{ContentBlock, TextContent};

fn main() {
    let block = ContentBlock::Text(TextContent {
        text: "Hello, World!".to_string(),
        annotations: None,
        meta: None,
    });

    println!("ContentBlock created successfully: {:?}", block);
}
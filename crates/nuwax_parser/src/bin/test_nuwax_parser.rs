use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use tokio;
use nuwax_parser::{V0FileData, V0FileSync};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    println!("🚀 Testing Nuwax parser...");

    // Load the test fixture
    let fixture_path = PathBuf::from("fixtures/v0-file.json");
    let json_content = if fixture_path.exists() {
        fs::read_to_string(&fixture_path)?
    } else {
        // Try alternative path
        let alt_path = PathBuf::from("../../../fixtures/v0-file.json");
        fs::read_to_string(&alt_path)?
    };

    println!("📝 Loaded test fixture from: {:?}", fixture_path);

    // Parse V0 file data
    let v0_data = V0FileData::from_json(&json_content)?;
    println!("✅ Parsed Nuwax file data, block_id: {}", v0_data.block_id);

    // Parse source content
    let parse_result = v0_data.parse_source()?;
    println!("📋 Found {} files in Nuwax format", parse_result.files.len());

    // Debug: show the first 500 characters of the source
    println!("🔍 Source preview (first 500 chars): {}", &v0_data.source[..500.min(v0_data.source.len())]);

    // Display file information
    for (i, file) in parse_result.files.iter().enumerate() {
        println!("File {}: {} ({})", i + 1, file.file_path.display(), file.file_type);
        println!("  - Merged: {}", file.is_merged);
        println!("  - Edit: {}", file.is_edit);
        println!("  - Quick Edit: {}", file.is_quick_edit);
        println!("  - Has URL: {}", file.url.is_some());
        println!("  - Content length: {} bytes", file.content.len());
        println!("  - Hash: {}", file.hash);
        println!();
    }

    // Test file synchronization
    let test_output_dir = PathBuf::from("./test_output");
    let file_sync = V0FileSync::new(&test_output_dir);

    println!("🔄 Testing file synchronization...");
    let synced_files = file_sync.sync_files(&v0_data).await?;
    println!("✅ Synced {} files to {:?}", synced_files.len(), test_output_dir);

    // Test reading project files
    println!("📖 Testing project file reading...");
    let project_files = file_sync.read_project_files(true).await?;
    println!("📋 Read {} project files (ignoring hidden directories)", project_files.len());

    // Test reverse conversion (project files to V0 format)
    println!("🔄 Testing V0 format generation...");
    let v0_format = nuwax_parser::generate_v0_format(&project_files)?;
    println!("✅ Generated V0 format with length: {} bytes", v0_format.len());

    // Save the generated V0 format for inspection
    let output_path = test_output_dir.join("generated_v0.json");
    fs::write(&output_path, v0_format)?;
    println!("💾 Saved generated V0 format to: {:?}", output_path);

    println!("🎉 All Nuwax parser tests completed successfully!");

    Ok(())
}
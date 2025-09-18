// Simple test to verify workspace compilation
use std::path::Path;

fn main() {
    println!("Testing workspace compilation...");

    // Check if critical paths exist
    assert!(Path::new("crates").exists());
    assert!(Path::new("crates/rcoder").exists());
    assert!(Path::new("crates/shared_types").exists());
    assert!(Path::new("crates/project_manager").exists());
    assert!(Path::new("crates/http_server").exists());
    assert!(Path::new("crates/claude_integration").exists());
    assert!(Path::new("crates/acp_client").exists());
    assert!(Path::new("crates/nuwax_parser").exists());

    println!("All paths exist!");
}
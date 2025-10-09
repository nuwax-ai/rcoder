use super::*;
use serde_json;

#[test]
fn test_source_code_serialization() {
    let source_code = ProjectSourceCode::new()
        .with_files(vec![
            FileInfo::new("test.txt")
                .with_contents("Hello, World!")
                .binary(false)
                .size_exceeded(false),
            FileInfo::new("image.png")
                .binary(true)
                .size_exceeded(false),
        ]);

    let json = serde_json::to_string(&source_code).unwrap();
    println!("Serialized: {}", json);

    let deserialized: ProjectSourceCode = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.files.len(), 2);
    assert_eq!(deserialized.files[0].name, "test.txt");
    assert_eq!(deserialized.files[0].contents, Some("Hello, World!".to_string()));
    assert_eq!(deserialized.files[1].name, "image.png");
    assert!(deserialized.files[1].binary);
    assert_eq!(deserialized.files[1].contents, None);
}

#[test]
fn test_file_info_builder() {
    let file = FileInfo::new("README.md")
        .with_contents("# Project Documentation\n\nThis is a README file.")
        .binary(false)
        .size_exceeded(false);

    assert_eq!(file.name, "README.md");
    assert_eq!(file.contents, Some("# Project Documentation\n\nThis is a README file.".to_string()));
    assert!(!file.binary);
    assert!(!file.size_exceeded);
}

#[test]
fn test_parse_actual_lovable_json() {
    // Test with a small subset of the actual lovable JSON
    let json_data = serde_json::json!({
        "files": [
            {
                "name": ".github",
                "binary": false,
                "sizeExceeded": false
            },
            {
                "name": ".gitignore",
                "contents": "# Logs\nlogs\n*.log",
                "binary": false,
                "sizeExceeded": false
            },
            {
                "name": "bun.lockb",
                "binary": true,
                "sizeExceeded": true
            }
        ]
    });

    let source_code: ProjectSourceCode = serde_json::from_value(json_data).unwrap();
    assert_eq!(source_code.files.len(), 3);

    let github_dir = &source_code.files[0];
    assert_eq!(github_dir.name, ".github");
    assert_eq!(github_dir.contents, None);
    assert!(!github_dir.binary);
    assert!(!github_dir.size_exceeded);

    let gitignore = &source_code.files[1];
    assert_eq!(gitignore.name, ".gitignore");
    assert_eq!(gitignore.contents, Some("# Logs\nlogs\n*.log".to_string()));
    assert!(!gitignore.binary);
    assert!(!gitignore.size_exceeded);

    let bun_lockb = &source_code.files[2];
    assert_eq!(bun_lockb.name, "bun.lockb");
    assert_eq!(bun_lockb.contents, None);
    assert!(bun_lockb.binary);
    assert!(bun_lockb.size_exceeded);
}
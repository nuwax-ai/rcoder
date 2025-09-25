use nuwax_parser::project_op::{ProjectReader, ProjectReadConfigBuilder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() != 2 {
        eprintln!("Usage: {} <project_path>", args[0]);
        std::process::exit(1);
    }

    let project_path = &args[1];

    // 创建默认配置的读取器
    let reader = ProjectReader::new();

    println!("Reading project from: {}", project_path);

    // 读取项目
    match reader.read_project(project_path) {
        Ok(project) => {
            println!("Successfully read project with {} files:", project.files.len());

            for file in &project.files {
                println!("  - {} (binary: {}, size_exceeded: {}, content_length: {})",
                    file.name,
                    file.binary,
                    file.size_exceeded,
                    file.contents.as_ref().map_or(0, |c| c.len())
                );
            }
        }
        Err(e) => {
            eprintln!("Error reading project: {}", e);
            std::process::exit(1);
        }
    }

    // 示例：使用自定义配置
    println!("\n--- Using custom configuration ---");
    let custom_config = ProjectReadConfigBuilder::default()
        .max_file_size((512 * 1024) as u64) // 512KB
        .include_hidden_files(true)
        .exclude_file_patterns(vec![r".*\.tmp$".to_string()]) // 排除所有 .tmp 文件
        .exclude_dir_patterns(vec![r".*\.idea$".to_string()]) // 排除 .idea 目录
        .build()
        .unwrap();

    let custom_reader = ProjectReader::with_config(custom_config);

    match custom_reader.read_project(project_path) {
        Ok(project) => {
            println!("Custom config read {} files", project.files.len());
        }
        Err(e) => {
            eprintln!("Error reading project with custom config: {}", e);
        }
    }

    // 示例：使用额外的文件排除（通过配置实现）
    println!("\n--- Using additional file exclusions ---");
    let exclude_config = ProjectReadConfigBuilder::default()
        .exclude_files(vec!["Cargo.toml".to_string(), "README.md".to_string()])
        .build()
        .unwrap();

    let exclude_reader = ProjectReader::with_config(exclude_config);
    match exclude_reader.read_project(project_path) {
        Ok(project) => {
            println!("With excludes read {} files", project.files.len());
            println!("Excluded files: {:?}", ["Cargo.toml", "README.md"]);
        }
        Err(e) => {
            eprintln!("Error reading project with excludes: {}", e);
        }
    }

    Ok(())
}
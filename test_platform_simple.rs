fn main() {
    println!("🧪 测试 Docker 平台自动检测功能");
    println!("==================================");

    // 模拟自动检测逻辑
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;

    println!("📱 当前系统架构: {} {}", os, arch);

    let detected_platform = match (os, arch) {
        ("macos", "aarch64") => "linux/arm64",
        ("linux", "aarch64") => "linux/arm64",
        ("macos", "x86_64") => "linux/amd64",
        ("linux", "x86_64") => "linux/amd64",
        ("windows", "x86_64") => "linux/amd64",
        (_, "arm64") => "linux/arm64",
        _ => "linux/amd64",
    };

    println!("🎯 自动检测平台: {}", detected_platform);

    // 检查环境变量
    if let Ok(env_platform) = std::env::var("DOCKER_DEFAULT_PLATFORM") {
        println!("🔧 环境变量平台: {}", env_platform);
        println!("✅ 最终使用平台: {}", env_platform);
    } else {
        println!("📋 无环境变量设置");
        println!("✅ 最终使用平台: {}", detected_platform);
    }

    // 测试镜像兼容性检查
    let test_images = ["latest", "latest-arm64", "latest-amd64"];
    println!("\n🔍 镜像兼容性测试:");

    for image_tag in test_images {
        let image_platform = if image_tag.contains("arm64") || image_tag.contains("aarch64") {
            "linux/arm64"
        } else if image_tag.contains("amd64") || image_tag.contains("x86_64") {
            "linux/amd64"
        } else {
            "unknown"
        };

        let current_platform = if let Ok(env_platform) = std::env::var("DOCKER_DEFAULT_PLATFORM") {
            env_platform
        } else {
            detected_platform.to_string()
        };

        let compatible = image_platform == "unknown" || current_platform == image_platform;
        println!(
            "  {} -> {} {}",
            image_tag,
            image_platform,
            if compatible { "✅" } else { "❌" }
        );
    }
}

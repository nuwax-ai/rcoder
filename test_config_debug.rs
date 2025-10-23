fn main() {
    println!("🧪 测试 DockerManagerConfig 自动检测");
    println!("=====================================");

    // 模拟调用 config_from_env 的逻辑
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;

    let detected_platform = match (os, arch) {
        ("macos", "aarch64") => "linux/arm64",
        ("linux", "aarch64") => "linux/arm64",
        ("macos", "x86_64") => "linux/amd64",
        ("linux", "x86_64") => "linux/amd64",
        ("windows", "x86_64") => "linux/amd64",
        (_, "arm64") => "linux/arm64",
        _ => "linux/amd64",
    };

    println!("📱 当前系统: {} {}", os, arch);
    println!("🎯 检测到的平台: {}", detected_platform);

    // 测试镜像选择
    let detected_image = match detected_platform {
        "linux/arm64" => "registry.yichamao.com/rcoder:latest-arm64",
        "linux/amd64" => "registry.yichamao.com/rcoder:latest-amd64",
        _ => "registry.yichamao.com/rcoder:latest",
    };

    println!("🏷️  选择的镜像: {}", detected_image);

    // 测试环境变量覆盖
    if let Ok(env_platform) = std::env::var("DOCKER_DEFAULT_PLATFORM") {
        println!("🔧 环境变量平台: {}", env_platform);
        println!("✅ 最终平台: {}", env_platform);
    } else {
        println!("📋 无环境变量，使用自动检测");
        println!("✅ 最终平台: {}", detected_platform);
    }

    // 模拟 DockerManagerConfig 的默认值
    let simulated_default_platform = detected_platform.to_string();
    let simulated_default_image = detected_image.to_string();

    println!("");
    println!("📋 DockerManagerConfig 默认值将是:");
    println!("  - default_platform: {}", simulated_default_platform);
    println!("  - default_image: {}", simulated_default_image);
}

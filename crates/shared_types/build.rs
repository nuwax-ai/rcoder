fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 使用 tonic-prost-build 的 configure 函数
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .out_dir("src/grpc")
        .compile_protos(
            &["proto/agent.proto"],
            &["proto"],
        )?;
    
    Ok(())
}

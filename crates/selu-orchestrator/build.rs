fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Use protox to compile without requiring an external protoc binary
    let file_descriptor = protox::compile(["proto/capability.proto"], ["proto/"])?;

    tonic_build::configure()
        .build_server(false) // orchestrator is a gRPC client only
        .build_client(true)
        .compile_fds(file_descriptor)?;

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true) // needed for the in-process mock server used in tests
        .build_client(true)
        .compile(&["../proto/execution.proto"], &["../proto"])?;
    Ok(())
}

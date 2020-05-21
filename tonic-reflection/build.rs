fn main() -> Result<(), Box<dyn std::error::Error>> {
    const OUT_DIR: &str = "src/proto";

    std::fs::create_dir_all(OUT_DIR)?;

    tonic_build::configure()
        .out_dir(OUT_DIR)
        .include_file_descriptor_set(true)
        .build_server(true)
        .build_client(false)
        .format(true)
        .compile(&["proto/reflection.proto"], &["proto/"])?;

    Ok(())
}

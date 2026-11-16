fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    let file_descriptors = protox::compile(
        [
            "exa/auth_pb/auth.proto",
            "exa/api_server_pb/api_server.proto",
        ],
        ["proto"],
    )?;
    prost_build::Config::new()
        .compile_well_known_types()
        .type_attribute(".", "#[derive(serde::Serialize)]")
        .include_file("windsurf_proto.rs")
        .compile_fds(file_descriptors)?;
    Ok(())
}

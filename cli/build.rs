fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../proto/aetherlynx.proto");
    tonic_prost_build::configure()
        .build_server(false)
        .compile_protos(&["../proto/aetherlynx.proto"], &["../proto"])?;
    Ok(())
}

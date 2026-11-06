fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../proto/aetherlynx.proto");
    // El cliente generado se usa en los tests de integración.
    tonic_prost_build::configure().compile_protos(&["../proto/aetherlynx.proto"], &["../proto"])?;
    Ok(())
}

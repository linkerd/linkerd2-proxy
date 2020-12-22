fn main() -> Result<(), Box<dyn std::error::Error>> {
    let files = &["proto/opaque.proto"];
    let dirs = &["proto"];

    prost_build::compile_protos(files, dirs)?;

    // recompile protobufs only if any of the proto files changes.
    for file in files {
        println!("cargo:rerun-if-changed={}", file);
    }

    Ok(())
}

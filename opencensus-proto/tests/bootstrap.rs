//! A test that regenerates the Rust protobuf bindings.
//!
//! It can be run via:
//!
//! ```no_run
//! cargo test -p opencensus-proto --test=bootstrap
//! ```

/// Generates protobuf bindings into src/gen and fails if the generated files do
/// not match those that are already checked into git
#[test]
fn bootstrap() {
    let out_dir = std::path::PathBuf::from(std::env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("gen");
    generate(&*out_dir);
    if changed(&*out_dir) {
        panic!("protobuf interfaces do not match generated sources");
    }
}

/// Generates protobuf bindings into the given directory
fn generate(out_dir: &std::path::Path) {
    let iface_files = &[
        "opencensus/proto/agent/common/v1/common.proto",
        "opencensus/proto/agent/trace/v1/trace_service.proto",
        "opencensus/proto/resource/v1/resource.proto",
        "opencensus/proto/trace/v1/trace_config.proto",
        "opencensus/proto/trace/v1/trace.proto",
    ];
    if let Err(error) = tonic_build::configure()
        .build_client(true)
        .build_server(false)
        .emit_rerun_if_changed(false)
        .out_dir(out_dir)
        .compile(iface_files, &["."])
    {
        panic!("failed to compile protobuf: {error}")
    }
}

/// Returns true if the given path contains files that have changed since the
/// last Git commit
fn changed(path: &std::path::Path) -> bool {
    let status = std::process::Command::new("git")
        .arg("diff")
        .arg("--exit-code")
        .arg("--")
        .arg(path)
        .status()
        .expect("failed to run git");
    !status.success()
}

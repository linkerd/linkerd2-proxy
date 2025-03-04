use std::path::{Path, PathBuf};

fn main() {
    generate_opentelemetry_protos();
    generate_opencensus_protos();
}

fn generate_opencensus_protos() {
    let opencensus_dir = get_proto_dir("opencensus-proto");

    let out_dir = opencensus_dir.join("src").join("gen");

    let iface_files = {
        let proto_dir = opencensus_dir.join("opencensus").join("proto");
        &[
            proto_dir.join("agent/common/v1/common.proto"),
            proto_dir.join("agent/trace/v1/trace_service.proto"),
            proto_dir.join("resource/v1/resource.proto"),
            proto_dir.join("trace/v1/trace_config.proto"),
            proto_dir.join("trace/v1/trace.proto"),
        ]
    };

    generate_protos(&out_dir, iface_files, &opencensus_dir);
}

fn generate_opentelemetry_protos() {
    let opentelemetry_dir = get_proto_dir("opentelemetry-proto");

    let out_dir = opentelemetry_dir.join("src").join("gen");

    let iface_files = {
        let proto_dir = opentelemetry_dir.join("opentelemetry").join("proto");
        &[
            proto_dir.join("collector/trace/v1/trace_service.proto"),
            proto_dir.join("common/v1/common.proto"),
            proto_dir.join("resource/v1/resource.proto"),
            proto_dir.join("trace/v1/trace.proto"),
        ]
    };

    generate_protos(&out_dir, iface_files, &opentelemetry_dir);
}

fn get_proto_dir(name: &str) -> PathBuf {
    let manifest_dir = std::path::PathBuf::from(std::env!("CARGO_MANIFEST_DIR"));
    manifest_dir.parent().unwrap().join(name)
}

fn generate_protos(out_dir: &Path, iface_files: &[PathBuf], includes: &Path) {
    if let Err(error) = tonic_build::configure()
        .build_client(true)
        .build_server(false)
        .emit_rerun_if_changed(false)
        .out_dir(out_dir)
        .compile_protos(iface_files, &[includes])
    {
        eprintln!("\nfailed to compile protos: {}", error);
    }
}

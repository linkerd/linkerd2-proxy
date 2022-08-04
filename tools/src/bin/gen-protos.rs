fn main() {
    let opencensus_dir = {
        let manifest_dir = std::path::PathBuf::from(std::env!("CARGO_MANIFEST_DIR"));
        manifest_dir.parent().unwrap().join("opencensus-proto")
    };

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
    if let Err(error) = tonic_build::configure()
        .build_client(true)
        .build_server(false)
        .out_dir(out_dir)
        .compile(iface_files, &[opencensus_dir])
    {
        eprintln!("\nfailed to compile protos: {}", error);
    }
}

extern crate tower_grpc_build;

fn main() {
    let iface_files = &["opencensus/proto/agent/trace/v1/trace_service.proto"];
    let dirs = &["."];

    tower_grpc_build::Config::new()
        .enable_client(true)
        .build(iface_files, dirs)
        .unwrap_or_else(|e| panic!("protobuf compilation failed: {}", e));

    // recompile protobufs only if any of the proto files changes.
    for file in iface_files {
        println!("cargo:rerun-if-changed={}", file);
    }
}

#![no_main]

#[cfg(fuzzing)]
use {libfuzzer_sys::fuzz_target, linkerd_transport_header::fuzz_logic::*};

#[cfg(fuzzing)]
fuzz_target!(|inp: TransportHeaderSpec| {
    // Don't enable tracing in `cluster-fuzz`, since we would emit verbose
    // traces for *every* generated fuzz input...
    let _trace = linkerd_tracing::test::with_default_filter("off");
    tracing::info!(spec = ?inp, "running with input");

    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(fuzz_entry_structured(inp));
});

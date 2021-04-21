#![no_main]

#[cfg(fuzzing)]
use {libfuzzer_sys::fuzz_target, linkerd_transport_header::fuzz_logic::*};

#[cfg(fuzzing)]
fuzz_target!(|inp: TransportHeaderSpec| {
    let _ = tracing_subscriber::fmt::try_init();
    tracing::info!(spec = ?inp, "running with input");

    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(fuzz_entry_structured(inp));
});

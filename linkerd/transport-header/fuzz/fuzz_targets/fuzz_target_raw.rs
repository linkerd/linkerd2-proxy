#![no_main]

#[cfg(fuzzing)]
use {libfuzzer_sys::fuzz_target, linkerd_transport_header::fuzz_logic::*};

#[cfg(fuzzing)]
fuzz_target!(|data: &[u8]| {
    let _trace = linkerd_tracing::test::trace_init();
    tracing::info!(?data, "running with input");

    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(fuzz_entry_raw(data));
});

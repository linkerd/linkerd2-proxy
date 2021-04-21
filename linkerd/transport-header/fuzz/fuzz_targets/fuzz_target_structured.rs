#![no_main]

#[cfg(fuzzing)]
use {libfuzzer_sys::fuzz_target, linkerd_transport_header::fuzz_logic::*};

#[cfg(fuzzing)]
fuzz_target!(|inp: TransportHeaderSpec| {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(fuzz_entry_structured(inp));
});

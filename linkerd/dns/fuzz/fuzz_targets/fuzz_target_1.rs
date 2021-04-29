#![no_main]

#[cfg(fuzzing)]
use libfuzzer_sys::fuzz_target;

#[cfg(fuzzing)]
fuzz_target!(|data: &[u8]| {
    let _trace = linkerd_tracing::test::trace_init();
    if let Ok(s) = std::str::from_utf8(data) {
        tracing::info!(data = ?s, "running with input");
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(linkerd_dns::fuzz_logic::fuzz_entry(s))
    }
});

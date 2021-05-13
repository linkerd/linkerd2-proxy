#![no_main]

#[cfg(fuzzing)]
use libfuzzer_sys::fuzz_target;

#[cfg(fuzzing)]
fuzz_target!(|data: &[u8]| {
    // Don't enable tracing in `cluster-fuzz`, since we would emit verbose
    // traces for *every* generated fuzz input...
    let _trace = linkerd_tracing::test::with_default_filter("off");
    if let Ok(s) = std::str::from_utf8(data) {
        tracing::info!(data = ?s, "running with input");
        linkerd_addr::fuzz_logic::fuzz_addr_1(s);
    }
});

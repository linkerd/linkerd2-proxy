#![no_main]

#[cfg(fuzzing)]
use libfuzzer_sys::fuzz_target;

#[cfg(fuzzing)]
fuzz_target!(|data: &[u8]| {
    // Don't enable tracing in `cluster-fuzz`, since we would emit verbose
    // traces for *every* generated fuzz input...
    let _trace = linkerd_tracing::test::with_default_filter("off");
    linkerd_tls::server::fuzz_logic::fuzz_entry(data);
});

#![no_main]

#[cfg(fuzzing)]
use libfuzzer_sys::fuzz_target;

#[cfg(fuzzing)]
fuzz_target!(|data: &[u8]| {
    let _trace = linkerd_tracing::test::trace_init();
    linkerd_tls::server::fuzz_logic::fuzz_entry(data);
});

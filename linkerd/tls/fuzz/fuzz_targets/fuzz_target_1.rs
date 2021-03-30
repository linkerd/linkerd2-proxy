#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    linkerd_tls::server::fuzz_logic::fuzz_entry(data);
});

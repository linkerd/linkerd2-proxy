#![no_main]
use libfuzzer_sys::fuzz_target;
use linkerd_transport_header::fuzz_logic::*;

fuzz_target!(|data: &[u8]| {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(fuzz_entry_raw(data));
});

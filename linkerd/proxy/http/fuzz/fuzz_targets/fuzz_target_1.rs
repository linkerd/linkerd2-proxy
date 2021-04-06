#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(linkerd_proxy_http::detect::fuzz_logic::fuzz_entry(data))
});


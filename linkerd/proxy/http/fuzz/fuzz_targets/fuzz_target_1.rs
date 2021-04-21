#![no_main]

#[cfg(fuzzing)]
use libfuzzer_sys::fuzz_target;

#[cfg(fuzzing)]
fuzz_target!(|data: &[u8]| {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(linkerd_proxy_http::detect::fuzz_logic::fuzz_entry(data))
});

#![no_main]

#[cfg(fuzzing)]
use libfuzzer_sys::fuzz_target;

#[cfg(fuzzing)]
fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(linkerd_dns::fuzz_logic::fuzz_entry(s))
    }
});

#![no_main]
use libfuzzer_sys::fuzz_target;


fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        linkerd_addr::fuzz_logic::fuzz_addr_1(s);
    }
});

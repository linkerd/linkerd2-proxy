#![no_main]
use libfuzzer_sys::fuzz_target;
use libfuzzer_sys::arbitrary::Arbitrary;
extern crate hyper;

#[derive(Debug, Arbitrary)]
struct TransportHeaderSpec {
        data: Vec<u8>,
        h1: Vec<u8>,
        h2: Vec<u8>,
        protocol: bool,
}

//fuzz_target!(|data: &[u8]| {
fuzz_target!(|inp: TransportHeaderSpec| {
    if let Ok(s) = std::str::from_utf8(&inp.data[..]) {
        if let Ok(h1_s) = std::str::from_utf8(&inp.h1[..]) {
            if let Ok(h2_s) = std::str::from_utf8(&inp.h2[..]) {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(linkerd_app_inbound::http::fuzz_logic::fuzz_entry_raw(s, h1_s, h2_s));
            }
        }
    }
});

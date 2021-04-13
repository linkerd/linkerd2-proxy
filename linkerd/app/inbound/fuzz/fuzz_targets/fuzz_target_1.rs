#![no_main]
use libfuzzer_sys::fuzz_target;
use libfuzzer_sys::arbitrary::Arbitrary;
extern crate hyper;

#[derive(Debug, Arbitrary)]
struct TransportHeaderSpec {
        uri: Vec<u8>,
        header1: Vec<u8>,
        header2: Vec<u8>,
        http_method: bool,
}

fuzz_target!(|inp: TransportHeaderSpec| {
    if let Ok(uri) = std::str::from_utf8(&inp.uri[..]) {
        if let Ok(header1_s) = std::str::from_utf8(&inp.header1[..]) {
            if let Ok(header2_s) = std::str::from_utf8(&inp.header2[..]) {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(linkerd_app_inbound::http::fuzz_logic::fuzz_entry_raw(uri, header1_s, header2_s, inp.http_method));
            }
        }
    }
});

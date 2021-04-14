#![no_main]
use libfuzzer_sys::arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

#[derive(Debug, Arbitrary)]
struct TransportHeaderSpec {
    uri: Vec<u8>,
    header_name: Vec<u8>,
    header_value: Vec<u8>,
    http_method: bool,
}

fuzz_target!(|inp: TransportHeaderSpec| {
    if let Ok(uri) = std::str::from_utf8(&inp.uri[..]) {
        if let Ok(header_name) = std::str::from_utf8(&inp.header_name[..]) {
            if let Ok(header_value) = std::str::from_utf8(&inp.header_value[..]) {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(linkerd_app_inbound::http::fuzz_logic::fuzz_entry_raw(
                    uri,
                    header_name,
                    header_value,
                    inp.http_method,
                ));
            }
        }
    }
});

#![no_main]
use libfuzzer_sys::fuzz_target;
use libfuzzer_sys::arbitrary::Arbitrary;

#[derive(Debug, Arbitrary)]
struct TransportHeaderSpec {
    data: Vec<u8>,
    port: u16,
    protocol: bool,
}

fuzz_target!(|inp: TransportHeaderSpec| {
    if let Ok(s) = std::str::from_utf8(&inp.data[..]) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let proto = if inp.protocol {
            linkerd_transport_header::SessionProtocol::Http2
        } else {
            linkerd_transport_header::SessionProtocol::Http1
        };
        rt.block_on(linkerd_transport_header::fuzz_logic::fuzz_entry_structured(s, inp.port, proto));
    }
});

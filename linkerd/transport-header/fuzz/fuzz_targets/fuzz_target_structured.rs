#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(
    |inp: linkerd_transport_header::fuzz_logic::TransportHeaderSpec| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(linkerd_transport_header::fuzz_logic::fuzz_entry_structured(
            inp,
        ));
    }
);

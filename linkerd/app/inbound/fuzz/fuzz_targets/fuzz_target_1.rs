#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|packets: Vec<linkerd_app_inbound::http::fuzz_logic::TransportHeaderSpec>| {
    if packets.len() == 0 {
        return;
    }

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(linkerd_app_inbound::http::fuzz_logic::fuzz_entry_raw(packets));
});

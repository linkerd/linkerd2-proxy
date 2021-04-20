#![no_main]
use libfuzzer_sys::fuzz_target;
use linkerd_app_inbound::http::fuzz_logic::*;

fuzz_target!(|requests: Vec<HttpRequestSpec>| {
    if requests.len() == 0 {
        return;
    }

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(fuzz_entry_raw(requests));
});

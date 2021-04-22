#![no_main]

#[cfg(fuzzing)]
use {libfuzzer_sys::fuzz_target, linkerd_app_inbound::http::fuzz_logic::*};

#[cfg(fuzzing)]
fuzz_target!(|requests: Vec<HttpRequestSpec>| {
    let _ = tracing_subscriber::fmt::try_init();
    tracing::info!(?requests, "running with input");
    if requests.len() == 0 {
        return;
    }

    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(fuzz_entry_raw(requests));
});

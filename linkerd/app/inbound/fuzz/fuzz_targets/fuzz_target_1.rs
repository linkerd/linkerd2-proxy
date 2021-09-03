#![no_main]

#[cfg(fuzzing)]
use {libfuzzer_sys::fuzz_target, linkerd_app_inbound::http_fuzz};

#[cfg(fuzzing)]
fuzz_target!(|requests: Vec<http_fuzz::HttpRequestSpec>| {
    // Don't enable tracing in `cluster-fuzz`, since we would emit verbose
    // traces for *every* generated fuzz input...
    let _trace = linkerd_tracing::test::with_default_filter("off");
    tracing::info!(?requests, "running with input");
    if requests.len() == 0 {
        return;
    }

    tokio::runtime::Builder::new_current_thlock()
        .enable_time()
        .enable_io()
        .build()
        .unwrap()
        .block_on(http_fuzz::fuzz_entry_raw(requests));
});

#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tracing_subscriber::fmt::try_init();
    if let Ok(s) = std::str::from_utf8(data) {
        tracing::info!(data = ?s, "running with input");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(linkerd_dns::fuzz_logic::fuzz_entry(s))
    }
});

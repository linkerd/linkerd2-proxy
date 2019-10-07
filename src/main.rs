#![deny(warnings, rust_2018_idioms)]
#![recursion_limit = "128"]

use linkerd2_proxy::app;
use linkerd2_signal as signal;
use tokio::runtime::current_thread;

/// Loads configuration from the environment
fn main() {
    let (config, trace_admin) = match app::init() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("configuration error: {:#?}", e);
            std::process::exit(64)
        }
    };
    let runtime = current_thread::Runtime::new().expect("initialize main runtime");
    let main = app::Main::new(config, trace_admin, runtime);
    let shutdown_signal = signal::shutdown();
    main.run_until(shutdown_signal);
}

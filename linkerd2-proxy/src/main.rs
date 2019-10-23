//! The main entrypoint for the proxy.

#![deny(warnings, rust_2018_idioms)]
#![recursion_limit = "128"]
#![type_length_limit = "1110183"]

use linkerd2_app::{Main, SysOrigDstAddr};
use linkerd2_signal as signal;

/// Loads configuration from the environment
fn main() {
    let (config, trace_admin) = match linkerd2_app::init() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("configuration error: {:#?}", e);
            std::process::exit(64)
        }
    };
    let main = Main::new(config, trace_admin, SysOrigDstAddr::default());
    let shutdown_signal = signal::shutdown();
    main.run_until(shutdown_signal);
}

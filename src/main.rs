#![deny(warnings)]
#![recursion_limit = "128"]

extern crate linkerd2_proxy;

#[macro_use]
extern crate tracing;
extern crate tokio;
extern crate tracing_fmt;

use std::process;

mod signal;

// Look in lib.rs.
fn main() {
    // Load configuration.
    let (config, trace_admin) = match linkerd2_proxy::app::init() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("configuration error: {:#?}", e);
            process::exit(64)
        }
    };
    // NOTE: eventually, this is where we would choose to use the threadpool
    //       runtime instead, if acting as an ingress proxy.
    let runtime = tokio::runtime::current_thread::Runtime::new().expect("initialize main runtime");
    let main =
        linkerd2_proxy::app::Main::new(config, trace_admin, linkerd2_proxy::SoOriginalDst, runtime);
    let shutdown_signal = signal::shutdown();
    main.run_until(shutdown_signal);
}

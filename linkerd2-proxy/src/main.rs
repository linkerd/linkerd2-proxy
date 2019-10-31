//! The main entrypoint for the proxy.

#![deny(warnings, rust_2018_idioms)]
#![recursion_limit = "128"]
#![type_length_limit = "1110183"]

use futures::{future, Future};
use linkerd2_app::{trace, Config};
use linkerd2_signal as signal;

fn main() {
    // Load configuration from the environment without binding ports.
    let config = match Config::try_from_env() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Invalid configuration: {}", e);
            const EX_USAGE: i32 = 64;
            std::process::exit(EX_USAGE);
        }
    };

    let log_level = trace::init().expect("log");

    tokio::runtime::current_thread::Runtime::new()
        .expect("main runtime")
        .block_on(future::lazy(move || {
            let main = config.build(log_level).expect("config");
            let drain = main.spawn();
            signal::shutdown().and_then(|()| drain.drain())
        }))
        .expect("main");
}

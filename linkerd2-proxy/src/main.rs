//! The main entrypoint for the proxy.

#![deny(warnings, rust_2018_idioms)]
#![recursion_limit = "128"]
#![type_length_limit = "1110183"]

use linkerd2_app::Main;
use linkerd2_signal as sig;

fn main() {
    // Load configuration from the environment without binding ports.
    let main = match Main::try_from_env() {
        Ok(main) => main,
        Err(e) => {
            eprintln!("Configuration error: {}", e);
            std::process::exit(64) // EX_USAGE
        }
    };

    // Bind server ports.
    let bound = match main.bind() {
        Ok(bound) => bound,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1)
        }
    };

    // Run the application until a shutdown signal is received.
    bound.run_until(sig::shutdown());
}

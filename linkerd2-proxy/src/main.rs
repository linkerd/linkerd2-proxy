//! The main entrypoint for the proxy.

#![deny(warnings, rust_2018_idioms)]
#![recursion_limit = "128"]
#![type_length_limit = "1110183"]

use futures::{future, Future};
use linkerd2_app::{trace, Config};
use linkerd2_signal as signal;
pub use tracing::{debug, error, info, warn};

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

    tokio::runtime::current_thread::Runtime::new()
        .expect("main runtime")
        .block_on(future::lazy(move || {
            let app = match trace::init().and_then(move |t| config.build(t)) {
                Ok(app) => app,
                Err(e) => {
                    eprintln!("Initialization failure: {}", e);
                    std::process::exit(1);
                }
            };

            info!("Admin interface on {}", app.admin_addr());
            info!("Inbound interface on {}", app.inbound_addr());
            info!("Outbound interface on {}", app.outbound_addr());

            match app.tap_addr() {
                None => info!("Tap DISABLED"),
                Some(addr) => info!("Tap interface on {}", addr),
            }

            match app.local_identity() {
                None => warn!("Identity is DISABLED"),
                Some(identity) => {
                    info!("Local identity is {}", identity.name());
                    let addr = app.identity_addr().expect("must have identity addr");
                    match addr.identity.value() {
                        None => info!("Identity verified via {}", addr.addr),
                        Some(identity) => {
                            info!("Identity verified via {} ({})", addr.addr, identity);
                        }
                    }
                }
            }

            let dst_addr = app.dst_addr();
            match dst_addr.identity.value() {
                None => info!("Destinations resolved via {}", dst_addr.addr),
                Some(identity) => {
                    info!("Destinations resolved via {} ({})", dst_addr.addr, identity)
                }
            }

            if let Some(oc) = app.opencensus_addr() {
                match oc.identity.value() {
                    None => info!("OpenCensus tracing collector at {}", oc.addr),
                    Some(identity) => {
                        info!("OpenCensus tracing collector at {} ({})", oc.addr, identity)
                    }
                }
            }

            let drain = app.spawn();
            signal::shutdown().and_then(|()| drain.drain())
        }))
        .expect("main");
}

//! The main entrypoint for the proxy.

#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![type_length_limit = "16289823"]

use linkerd_app::{core::transport::BindTcp, trace, Config};
use linkerd_signal as signal;
use tokio::sync::mpsc;
pub use tracing::{debug, error, info, warn};

#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

mod rt;

const EX_USAGE: i32 = 64;

fn main() {
    let trace = match trace::init() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Invalid logging configuration: {}", e);
            std::process::exit(EX_USAGE);
        }
    };

    // Load configuration from the environment without binding ports.
    let config = match Config::try_from_env() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Invalid configuration: {}", e);
            std::process::exit(EX_USAGE);
        }
    };

    // Builds a runtime with the appropriate number of cores:
    // `LINKERD2_PROXY_CORES` env or the number of available CPUs (as provided
    // by cgroups, when possible).
    rt::build().block_on(async move {
        let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel();
        let bind = BindTcp::with_orig_dst();
        let app = match config
            .build(bind, bind, BindTcp::default(), shutdown_tx, trace)
            .await
        {
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
                    Some(tls) => {
                        info!("Identity verified via {} ({})", addr.addr, tls.server_id);
                    }
                }
            }
        }

        let dst_addr = app.dst_addr();
        match dst_addr.identity.value() {
            None => info!("Destinations resolved via {}", dst_addr.addr),
            Some(tls) => info!(
                "Destinations resolved via {} ({})",
                dst_addr.addr, tls.server_id
            ),
        }

        if let Some(oc) = app.opencensus_addr() {
            match oc.identity.value() {
                None => info!("OpenCensus tracing collector at {}", oc.addr),
                Some(tls) => {
                    info!(
                        "OpenCensus tracing collector at {} ({})",
                        oc.addr, tls.server_id
                    )
                }
            }
        }

        let drain = app.spawn();
        tokio::select! {
            _ = signal::shutdown() => {
                info!("Received shutdown signal");
            }
            _ = shutdown_rx.recv() => {
                info!("Received shutdown via admin interface");
            }
        }
        drain.drain().await;
    });
}

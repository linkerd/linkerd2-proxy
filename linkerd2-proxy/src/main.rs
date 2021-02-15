//! The main entrypoint for the proxy.

#![deny(warnings, rust_2018_idioms)]
#![type_length_limit = "16289823"]

use linkerd_app::{core::svc, trace, Config};
use linkerd_signal as signal;
pub use tracing::{debug, error, info, warn};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod rt;

fn main() {
    let trace = trace::init();

    // Load configuration from the environment without binding ports.
    let config = match Config::try_from_env() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Invalid configuration: {}", e);
            const EX_USAGE: i32 = 64;
            std::process::exit(EX_USAGE);
        }
    };

    rt::build().block_on(async move {
        let mut app = match async move { config.build(svc::ConnectTcp::new(), trace?).await }.await
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

        let mut shutdown = app.take_shutdown().expect("Shutdown must be available");
        let drain = app.spawn();
        tokio::select! {
            _ = signal::shutdown() => {
                info!("Received shutdown signal");
            }
            _ = shutdown.recv() => {
                info!("Received shutdown via admin interface");
            }
        }
        drain.drain().await;
    });
}

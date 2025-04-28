//! The main entrypoint for the proxy.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]
#![recursion_limit = "256"]

// Emit a compile-time error if no TLS implementations are enabled. When adding
// new implementations, add their feature flags here!
#[cfg(not(any(
    feature = "meshtls-boring",
    feature = "meshtls-rustls-ring",
    feature = "meshtls-rustls-aws-lc",
    feature = "meshtls-rustls-aws-lc-fips"
)))]
compile_error!(
    "at least one of the following TLS implementations must be enabled: 'meshtls-boring', 'meshtls-rustls'"
);

use linkerd_app::{trace, BindTcp, Config, BUILD_INFO};
use linkerd_signal as signal;
use tokio::{sync::mpsc, time};
use tracing::{debug, info, warn};

#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

mod rt;

const EX_USAGE: i32 = 64;

fn main() {
    let trace = match trace::Settings::from_env().init() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Invalid logging configuration: {}", e);
            std::process::exit(EX_USAGE);
        }
    };

    info!(
        "{profile} {version} ({sha}) by {vendor} on {date}",
        date = BUILD_INFO.date,
        sha = BUILD_INFO.git_sha,
        version = BUILD_INFO.version,
        profile = BUILD_INFO.profile,
        vendor = BUILD_INFO.vendor,
    );

    let mut metrics = linkerd_metrics::prom::Registry::default();

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
        // Spawn a task to run in the background, exporting runtime metrics at a regular interval.
        rt::spawn_metrics_exporter(&mut metrics);

        let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel();
        let shutdown_grace_period = config.shutdown_grace_period;

        let bind_in = BindTcp::with_orig_dst();
        let bind_out = BindTcp::dual_with_orig_dst();
        let app = match config
            .build(
                bind_in,
                bind_out,
                BindTcp::default(),
                shutdown_tx,
                trace,
                metrics,
            )
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
        if let Some(addr) = app.outbound_addr_additional() {
            info!("Outbound interface on {addr}");
        }

        match app.tap_addr() {
            None => info!("Tap DISABLED"),
            Some(addr) => info!("Tap interface on {}", addr),
        }

        // TODO distinguish ServerName and Identity.
        info!("SNI is {}", app.local_server_name());
        info!("Local identity is {}", app.local_tls_id());

        let dst_addr = app.dst_addr();
        match dst_addr.identity.value() {
            None => info!("Destinations resolved via {}", dst_addr.addr),
            Some(tls) => info!(
                "Destinations resolved via {} ({})",
                dst_addr.addr, tls.server_id
            ),
        }

        if let Some(tracing) = app.tracing_addr() {
            match tracing.identity.value() {
                None => info!("Tracing collector at {}", tracing.addr),
                Some(tls) => {
                    info!("Tracing collector at {} ({})", tracing.addr, tls.server_id)
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
        match time::timeout(shutdown_grace_period, drain.drain()).await {
            Ok(()) => debug!("Shutdown completed gracefully"),
            Err(_) => warn!(
                "Graceful shutdown did not complete in {shutdown_grace_period:?}, terminating now"
            ),
        }
    });
}

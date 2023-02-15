//! The main entrypoint for the proxy.
//!
//! # linkerd2-proxy Developer Documentation
//!
//! ## Table of Contents
//!
//! 1. **Introduction**
//!     1. [Background](#background)
//!     2. [Navigating the Codebase](#navigating-the-codebase)
//! 2. [linkerd-stack](../linkerd-stack)
//! 3. [linkerd-app](../linkerd-app)
//!     1. [linkerd-app-outbound](../linkerd-app-outbound)
//!     2. [linkerd-app-inbound](../linkerd-app-inbound)
//!
//! This documentation is intended for developers working in the
//! `linkerd2-proxy` repo. This is *not* documentation about how to use and
//! operate Linkerd; for such documentation, see [linkerd.io/docs][docs-site]
//! instead. Similarly, this is not intended as complete API documentation for
//! every Linkerd crate; these crates are used as part of the Linkerd proxy
//! application, and are not intended as general purpose libraries. Instead,
//! this documentation aims to introduce developers working on the proxy to the
//! core concepts and architecture of the project.
//!
//! The proxy developer documentation assumes that the reader has an
//! intermediate working knowledge of the Rust programming language, and at
//! least some familiarity with general networking and distributed systems
//! principles. An introduction to programming in Rust is out of scope for this
//! document; readers who are new to Rust may wish to consult the book [_The Rust
//! Programming Language_][trpl], [Comprehensive Rust][comp-rust], or [other
//! resources][learn-rust] before continuing.
//!
//! Similarly, an understanding of Linkerd, Kubernetes, and service mesh in
//! general is also a prerequisite. The Linkerd documentation provides [an
//! introduction to service mesh][whats-mesh] for readers unfamiliar with these
//! systems.
//!
//! [docs-site]: https://linkerd.io/docs
//! [trpl]: https://doc.rust-lang.org/stable/book/
//! [comp-rust]: https://google.github.io/comprehensive-rust/
//! [learn-rust]: https://www.rust-lang.org/learn
//! [whats-mesh]: https://linkerd.io/what-is-a-service-mesh/
//!
//! ### Background
//!
//! In order to understand the proxy, it is important to understand some of the
//! key library dependencies used in its implementation. In particular,
//! [`tokio`] and [`tower`].
//!
//! Tokio is an asynchronous runtime for Rust applications. For a basic
//! introduction to Tokio that also introduces asynchronous programming in Rust,
//! see [the Tokio tutorial][tokio-tutorial]. The [the Async Rust
//! book][async-book] may also be useful background reading.
//!
//! Tower is a library of composable components for network applications.
//! The core abstraction of Tower, and by extension, the core abstraction of the
//! Linkerd proxy codebase, is the [`Service` trait]. A `Service` represents an
//! asynchronous function from a request value to a response value. A `Service`
//! which represents a network client or server can be composed with other
//! `Service`s that transform either the request value, the response value, or
//! both, representing middleware middleware implement various behaviors in a
//! generic way.
//!
//! Tower's design is strongly inspired by that of [Finagle], a similar system
//! implemented in Scala. The paper ["Your Service as a Function"][funsrv], by
//! Marius Eriksen, introduces the core concepts behind this design. While the
//! paper focuses on Finagle in particular, and code examples are given in Scala,
//! much of the content is applicable to Tower as well. For a gentle
//! introduction to Tower's `Service` trait, with a discussion of the design
//! decisions and tradeoffs behind it, I strongly recommend the blog post
//! ["Inventing the `Service` Trait"][inventing-svc], by David Pedersen.
//!
//! ### Navigating the Codebase
//!
//! The `linkerd2-proxy` codebase is split into a large number of crates (Rust
//! packages) which contain individual pieces of the proxy. Many of these crates
//! are additionally grouped into sub-directories.
//!
//! [`tokio`]: https://tokio.rs
//! [`tower`]: https://github.com/tower-rs/tower
//! [tokio-tutorial]: https://tokio.rs/tokio/tutorial
//! [async-book]: https://rust-lang.github.io/async-book/
//! [`Service` trait]: https://docs.rs/tower/latest/tower/trait.Service.html
//! [Finagle]: https://twitter.github.io/finagle/
//! [funsrv]: https://monkey.org/~marius/funsrv.pdf
//! [inventing-svc]: https://tokio.rs/blog/2021-05-14-inventing-the-service-trait

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]
#![recursion_limit = "256"]

// Emit a compile-time error if no TLS implementations are enabled. When adding
// new implementations, add their feature flags here!
#[cfg(not(any(feature = "meshtls-boring", feature = "meshtls-rustls")))]
compile_error!(
    "at least one of the following TLS implementations must be enabled: 'meshtls-boring', 'meshtls-rustls'"
);

use linkerd_app::{
    core::{telemetry::StartTime, transport::BindTcp},
    trace, Config,
};
use linkerd_signal as signal;
use tokio::{sync::mpsc, time};
pub use tracing::{debug, error, info, warn};

#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

mod rt;

const EX_USAGE: i32 = 64;

fn main() {
    let start_time = StartTime::now();
    let trace = match trace::Settings::from_env(start_time.into()).init() {
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
        let shutdown_grace_period = config.shutdown_grace_period;

        let bind = BindTcp::with_orig_dst();
        let app = match config
            .build(
                bind,
                bind,
                BindTcp::default(),
                shutdown_tx,
                trace,
                start_time,
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

        match app.tap_addr() {
            None => info!("Tap DISABLED"),
            Some(addr) => info!("Tap interface on {}", addr),
        }

        info!("Local identity is {}", app.local_identity());
        let addr = app.identity_addr();
        match addr.identity.value() {
            None => info!("Identity verified via {}", addr.addr),
            Some(tls) => {
                info!("Identity verified via {} ({})", addr.addr, tls.server_id);
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
        match time::timeout(shutdown_grace_period, drain.drain()).await {
            Ok(()) => debug!("Shutdown completed gracefully"),
            Err(_) => warn!(
                "Graceful shutdown did not complete in {shutdown_grace_period:?}, terminating now"
            ),
        }
    });
}

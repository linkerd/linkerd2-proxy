use tokio::runtime::{Builder, Runtime};
use tracing::{info, warn};

#[cfg(feature = "multicore")]
pub(crate) fn build() -> Runtime {
    // The proxy creates an additional admin thread, but it would be wasteful to
    // allocate a whole core to it; so we let the main runtime consume all
    // available cores. The number of available cores is determined by checking
    // the environment or by inspecting the host or cgroups.
    //
    // The basic scheduler is used when the threaded scheduler would provide no
    // benefit.
    let mut cores = std::env::var("LINKERD2_PROXY_CORES")
        .ok()
        .and_then(|v| {
            let opt = v.parse::<usize>().ok().filter(|n| *n > 0);
            if opt.is_none() {
                warn!(LINKERD2_PROXY_CORES = %v, "Ignoring invalid configuration");
            }
            opt
        })
        .unwrap_or(0);

    let cpus = num_cpus::get();
    debug_assert!(cpus > 0, "At least one CPU must be available");
    if cores > cpus {
        warn!(
            cpus,
            LINKERD2_PROXY_CORES = cores,
            "Ignoring configuration due to insufficient resources"
        );
        cores = cpus;
    }

    match cores {
        // `0` is unexpected, but it's a wild world out there.
        0 | 1 => {
            info!("Using single-threaded proxy runtime");
            Builder::new_current_thread()
                .enable_all()
                .thread_name("proxy")
                .build()
                .expect("failed to build basic runtime!")
        }
        num_cpus => {
            info!(%cores, "Using multi-threaded proxy runtime");
            Builder::new_multi_thread()
                .enable_all()
                .thread_name("proxy")
                .worker_threads(num_cpus)
                .max_blocking_threads(num_cpus)
                .build()
                .expect("failed to build threaded runtime!")
        }
    }
}

#[cfg(not(feature = "multicore"))]
pub(crate) fn build() -> Runtime {
    Builder::new()
        .enable_all()
        .thread_name("proxy")
        .basic_scheduler()
        .build()
        .expect("failed to build basic runtime!")
}

/// Spawns a task to scrape metrics for the given runtime at a regular interval.
///
/// Note that this module requires unstable tokio functionality that must be
/// enabled via the `tokio_unstable` feature. When it is not enabled, no metrics
/// will be registered.
///
/// `RUSTFLAGS="--cfg tokio_unstable"` must be set at build-time to use this feature.
pub fn spawn_metrics_exporter(registry: &mut linkerd_metrics::prom::Registry) {
    #[cfg(tokio_unstable)]
    {
        use {std::time::Duration, tracing::Instrument};

        /// The fixed interval at which tokio runtime metrics are updated.
        //
        //  TODO(kate): perhaps this could be configurable eventually. for now, it's hard-coded.
        const INTERVAL: Duration = Duration::from_secs(1);

        let mut interval = tokio::time::interval(INTERVAL);

        let registry = registry.sub_registry_with_prefix("tokio_rt");
        let runtime = tokio::runtime::Handle::current();
        let metrics = kubert_prometheus_tokio::Runtime::register(registry, runtime);

        tokio::spawn(
            async move { metrics.updated(&mut interval).await }
                .instrument(tracing::info_span!("kubert-prom-tokio-rt")),
        );
    }
    #[cfg(not(tokio_unstable))]
    {
        tracing::debug!("Tokio runtime metrics cannot be monitored without the tokio_unstable cfg");
    }
}

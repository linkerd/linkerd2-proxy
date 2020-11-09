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
                .max_threads(num_cpus)
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

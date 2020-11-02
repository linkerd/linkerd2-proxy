use tokio::runtime::{Builder, Runtime};

#[cfg(feature = "multicore")]
pub(crate) fn build() -> Runtime {
    // The proxy creates an additional admin thread, but it would be wasteful to
    // allocate a whole core to it; so we let the main runtime consume all
    // available cores. The number of available cores is determined by checking
    // the environment or by inspecting the host or cgroups.
    //
    // The basic scheduler is used when the threaded scheduler would provide no
    // benefit.
    let cpus = std::env::var("LINKERD2_PROXY_CORES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(|| num_cpus::get());
    match cpus {
        // `0` is unexpected, but it's a wild world out there.
        0 | 1 => Builder::new()
            .enable_all()
            .thread_name("proxy")
            .basic_scheduler()
            .build()
            .expect("failed to build basic runtime!"),
        num_cpus => Builder::new()
            .enable_all()
            .thread_name("proxy")
            .threaded_scheduler()
            .core_threads(num_cpus)
            .max_threads(num_cpus)
            .build()
            .expect("failed to build threaded runtime!"),
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

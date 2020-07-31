use tokio::runtime::{Builder, Runtime};

#[cfg(feature = "multicore")]
pub(crate) fn build() -> Runtime {
    // The proxy creates an additional admin thread, but it would be wasteful to allocate a whole
    // core to it; so we let the main runtime consume all cores the process has. The basic scheduler
    // is used when the threaded scheduler would provide no benefit.
    match num_cpus::get() {
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
        .expect("failed to build threaded runtime!")
}

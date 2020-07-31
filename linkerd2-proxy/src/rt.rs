use tokio::runtime::{self, Runtime};

#[cfg(feature = "multicore")]
pub(crate) fn build() -> Runtime {
    let builder = runtime::Builder::new()
        .enable_all()
        .thread_name("linkerd2-proxy-worker");
    let num_cpus = num_cpus::get();
    if num_cpus > 1 {
        builder
            .threaded_scheduler()
            .core_threads(num_cpus)
            .build()
            .expect("failed to build multithreaded runtime!")
    } else {
        builder
            .basic_scheduler()
            .build()
            .expect("failed to build single-threaded runtime!")
    }
}

#[cfg(not(feature = "multicore"))]
pub(crate) fn build() -> Runtime {
    runtime::Builder::new()
        .enable_all()
        .thread_name("linkerd2-proxy-worker")
        .basic_scheduler()
        .build()
        .expect("failed to build single-threaded runtime!")
}

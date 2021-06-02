use super::*;

/// By default, disable logging in modules that are expected to error in tests.
pub const DEFAULT_LOG: &str = "warn,\
                           linkerd=debug,\
                           linkerd_proxy_http=error,\
                           linkerd_proxy_transport=error";

pub fn trace_subscriber(default: impl ToString) -> (Dispatch, Handle) {
    let log_level = std::env::var("LINKERD2_PROXY_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| default.to_string());
    // This may fail, since the global log compat layer may have been
    // initialized by another test.
    let _ = init_log_compat();
    Settings::for_test(log_level, "".into()).build()
}

pub fn with_default_filter(default: impl ToString) -> (tracing::dispatcher::DefaultGuard, Handle) {
    let (d, handle) = trace_subscriber(default);
    let default = tracing::dispatcher::set_default(&d);
    (default, handle)
}

pub fn trace_init() -> (tracing::dispatcher::DefaultGuard, Handle) {
    with_default_filter(DEFAULT_LOG)
}

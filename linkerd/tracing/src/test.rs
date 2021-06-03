use super::*;

/// By default, disable logging in modules that are expected to error in tests.
pub const DEFAULT_LOG: &str = "warn,\
                           linkerd=debug,\
                           linkerd_proxy_http=error,\
                           linkerd_proxy_transport=error";

pub fn trace_subscriber(default: impl ToString) -> (Dispatch, Handle) {
    let log_level = env::var("LINKERD2_PROXY_LOG")
        .or_else(|_| env::var("RUST_LOG"))
        .unwrap_or_else(|_| default.to_string());
    let log_format = env::var("LINKERD2_PROXY_LOG_FORMAT").unwrap_or_else(|_| "PLAIN".to_string());
    env::set_var("LINKERD2_PROXY_LOG_FORMAT", &log_format);
    // This may fail, since the global log compat layer may have been
    // initialized by another test.
    let _ = init_log_compat();
    Settings::for_test(log_level, log_format).build()
}

pub fn with_default_filter(default: impl ToString) -> tracing::dispatcher::DefaultGuard {
    let (d, _) = trace_subscriber(default);
    tracing::dispatcher::set_default(&d)
}

pub fn trace_init() -> tracing::dispatcher::DefaultGuard {
    with_default_filter(DEFAULT_LOG)
}

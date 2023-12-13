pub use linkerd_proxy_balance::*;
pub use tower::load::CompleteOnResponse;

pub type NewBalancePeakEwma<Req, X, R, N> =
    linkerd_proxy_balance::NewBalancePeakEwma<CompleteOnResponse, Req, X, R, N>;

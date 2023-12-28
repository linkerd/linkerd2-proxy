pub use linkerd_proxy_balance::*;
pub use tower::load::CompleteOnResponse;

pub type NewBalance<Req, X, R, N> =
    linkerd_proxy_balance::NewBalance<CompleteOnResponse, Req, X, R, N>;

pub use hyper_balance::{PendingUntilFirstData, PendingUntilFirstDataBody};
pub use linkerd_proxy_balance::*;

pub type Body<B> = PendingUntilFirstDataBody<peak_ewma::Handle, B>;

pub type NewBalance<B, X, R, N> =
    linkerd_proxy_balance::NewBalance<PendingUntilFirstData, http::Request<B>, X, R, N>;

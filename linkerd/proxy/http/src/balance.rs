pub use hyper_balance::{PendingUntilFirstData, PendingUntilFirstDataBody};
pub use linkerd_proxy_balance::*;

pub type Body<B> = PendingUntilFirstDataBody<Handle, B>;

pub type NewBalancePeakEwma<B, R, N> =
    linkerd_proxy_balance::NewBalancePeakEwma<PendingUntilFirstData, http::Request<B>, R, N>;

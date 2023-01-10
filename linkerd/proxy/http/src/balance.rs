use hyper_balance::{PendingUntilFirstData, PendingUntilFirstDataBody};
use linkerd_proxy_balance as balance;

pub type Body<B> = PendingUntilFirstDataBody<balance::Handle, B>;

pub type EwmaConfig = balance::EwmaConfig;

pub type NewBalancePeakEwma<B, R, N> =
    balance::NewBalancePeakEwma<PendingUntilFirstData, http::Request<B>, R, N>;

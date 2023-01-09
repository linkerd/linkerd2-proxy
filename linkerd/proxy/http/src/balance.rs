use hyper_balance::{PendingUntilFirstData, PendingUntilFirstDataBody};
use linkerd_proxy_balance as balance;

pub type Body<B> = PendingUntilFirstDataBody<balance::Handle, B>;

pub type EwmaConfig = balance::EwmaConfig;

pub type NewP2cPeakEwma<B, R, N> =
    balance::NewP2cPeakEwma<PendingUntilFirstData, http::Request<B>, R, N>;

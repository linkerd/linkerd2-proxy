use linkerd_proxy_balance as balance;
use tower::load::CompleteOnResponse;

pub type EwmaConfig = balance::EwmaConfig;

pub type NewP2cPeakEwma<Req, R, N> = balance::NewP2cPeakEwma<CompleteOnResponse, Req, R, N>;

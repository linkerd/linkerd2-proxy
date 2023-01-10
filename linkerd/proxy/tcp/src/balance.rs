use linkerd_proxy_balance as balance;
use tower::load::CompleteOnResponse;

pub type EwmaConfig = balance::EwmaConfig;

pub type NewBalancePeakEwma<Req, R, N> = balance::NewBalancePeakEwma<CompleteOnResponse, Req, R, N>;

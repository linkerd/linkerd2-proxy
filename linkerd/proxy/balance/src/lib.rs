mod gauge_endpoints;
mod pool;

pub use self::{
    gauge_endpoints::{EndpointsGauges, NewGaugeEndpoints},
    pool::*,
};
pub use tower::load::peak_ewma;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct EwmaConfig {
    pub default_rtt: std::time::Duration,
    pub decay: std::time::Duration,
}

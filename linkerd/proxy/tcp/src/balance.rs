use linkerd_error::Error;
use linkerd_stack::layer;
use rand::thread_rng;
use std::{hash::Hash, time::Duration};
pub use tower::{
    balance::p2c::Balance,
    load::{Load, PeakEwmaDiscover},
};
use tower::{discover::Discover, load::CompleteOnResponse};

/// Produces a PeakEWMA balancer that uses connect latency (and pending
/// connections) as its load metric.
pub fn layer<T, D>(
    default_rtt: Duration,
    decay: Duration,
) -> impl tower::layer::Layer<D, Service = Balance<PeakEwmaDiscover<D, CompleteOnResponse>, T>> + Clone
where
    D: Discover,
    D::Key: Hash,
    D::Service: tower::Service<T>,
    <D::Service as tower::Service<T>>::Error: Into<Error>,
{
    layer::mk(move |discover| {
        let loaded =
            PeakEwmaDiscover::new(discover, default_rtt, decay, CompleteOnResponse::default());
        Balance::from_rng(loaded, &mut thread_rng()).expect("RNG must be valid")
    })
}

use linkerd2_error::Error;
use linkerd2_stack::layer;
use rand::{rngs::SmallRng, SeedableRng};
use std::{hash::Hash, time::Duration};
pub use tower::{
    balance::p2c::Balance,
    load::{Load, PeakEwmaDiscover},
};
use tower::{discover::Discover, load::CompleteOnResponse};

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
    let rng = SmallRng::from_entropy();
    layer::mk(move |discover| {
        let loaded =
            PeakEwmaDiscover::new(discover, default_rtt, decay, CompleteOnResponse::default());
        Balance::from_rng(loaded, rng.clone()).expect("RNG must be valid")
    })
}

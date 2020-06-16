use crate::Error;
use http;
use hyper::body::HttpBody;
pub use hyper_balance::{PendingUntilFirstData, PendingUntilFirstDataBody};
use rand::{rngs::SmallRng, SeedableRng};
use std::{hash::Hash, marker::PhantomData, time::Duration};
use tower::discover::Discover;
pub use tower::{
    balance::p2c::Balance,
    load::{Load, PeakEwmaDiscover},
};

/// Configures a stack to resolve `T` typed targets to balance requests over
/// `M`-typed endpoint stacks.
#[derive(Debug)]
pub struct Layer<A, B> {
    decay: Duration,
    default_rtt: Duration,
    rng: SmallRng,
    _marker: PhantomData<fn(A) -> B>,
}

// === impl Layer ===

pub fn layer<A, B>(default_rtt: Duration, decay: Duration) -> Layer<A, B> {
    Layer {
        decay,
        default_rtt,
        rng: SmallRng::from_entropy(),
        _marker: PhantomData,
    }
}

impl<A, B> Clone for Layer<A, B> {
    fn clone(&self) -> Self {
        Self {
            decay: self.decay,
            default_rtt: self.default_rtt,
            rng: self.rng.clone(),
            _marker: PhantomData,
        }
    }
}

impl<D, S, A, B> tower::layer::Layer<D> for Layer<A, B>
where
    A: HttpBody,
    B: HttpBody,
    D: Discover<Service = S>,
    D::Key: Hash,
    S: tower::Service<http::Request<A>, Response = http::Response<B>>,
    S::Error: Into<Error>,
    Balance<PeakEwmaDiscover<D, PendingUntilFirstData>, http::Request<A>>:
        tower::Service<http::Request<A>>,
{
    type Service = Balance<PeakEwmaDiscover<D, PendingUntilFirstData>, http::Request<A>>;

    fn layer(&self, discover: D) -> Self::Service {
        let instrument = PendingUntilFirstData::default();
        let loaded = PeakEwmaDiscover::new(discover, self.default_rtt, self.decay, instrument);
        Balance::new(loaded, self.rng.clone())
    }
}

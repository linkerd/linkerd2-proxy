// A `NewService` called `NewClassifyGate` that builds a
// `BroadcastClassification` and a `Gate` so that the `gate::Tx` can be
// controlled by the `BroadcastClassification`'s `Tx`.

use crate::classify::{BroadcastClassification, ClassifyResponse};
use linkerd_stack::{gate, layer, ExtractParam, Gate, NewService};
use std::marker::PhantomData;
use tokio::sync::mpsc;

pub use linkerd_stack::gate::{Rx, State, Tx};

#[derive(Clone, Debug)]
pub struct Params<C> {
    pub responses: mpsc::Sender<C>,
    pub gate: gate::Rx,
}

pub struct NewClassifyGateSet<C, P, X, N> {
    inner: N,
    extract: X,
    _marker: PhantomData<fn() -> (P, C)>,
}

pub struct NewClassifyGate<C, X, N> {
    inner: N,
    extract: X,
    _marker: PhantomData<fn() -> C>,
}

// === impl NewClassifyGateSet ===

impl<C, P, X: Clone, N> NewClassifyGateSet<C, P, X, N> {
    pub fn new(extract: X, inner: N) -> Self {
        Self {
            inner,
            extract,
            _marker: PhantomData,
        }
    }

    pub fn layer_via(extract: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(extract.clone(), inner))
    }
}

impl<C, P, N> NewClassifyGateSet<C, P, (), N> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<T, C, P, X, N> NewService<T> for NewClassifyGateSet<C, P, X, N>
where
    P: Clone,
    X: ExtractParam<P, T>,
    N: NewService<T>,
{
    type Service = NewClassifyGate<C, P, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let new_params = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);
        NewClassifyGate::new(new_params, inner)
    }
}

impl<C, P, X: Clone, N: Clone> Clone for NewClassifyGateSet<C, P, X, N> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            extract: self.extract.clone(),
            _marker: PhantomData,
        }
    }
}

// === impl NewClassifyGate ===

impl<C, X: Clone, N> NewClassifyGate<C, X, N> {
    pub fn new(extract: X, inner: N) -> Self {
        Self {
            inner,
            extract,
            _marker: PhantomData,
        }
    }

    pub fn layer_via(extract: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(extract.clone(), inner))
    }
}

impl<C, N> NewClassifyGate<C, (), N> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<T, C, X, N> NewService<T> for NewClassifyGate<C, X, N>
where
    C: ClassifyResponse,
    X: ExtractParam<Params<C::Class>, T>,
    N: NewService<T>,
{
    type Service = Gate<BroadcastClassification<C, N::Service>>;

    fn new_service(&self, target: T) -> Self::Service {
        let Params { responses, gate } = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);
        Gate::new(gate, BroadcastClassification::new(responses, inner))
    }
}

impl<C, X: Clone, N: Clone> Clone for NewClassifyGate<C, X, N> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            extract: self.extract.clone(),
            _marker: PhantomData,
        }
    }
}

// === impl Params ===

impl<C> Params<C> {
    pub fn channel(capacity: usize) -> (Self, gate::Tx, mpsc::Receiver<C>) {
        let (gate_tx, gate_rx) = gate::channel();
        let (rsps_tx, rsps_rx) = mpsc::channel(capacity);
        let prms = Self {
            gate: gate_rx,
            responses: rsps_tx,
        };
        (prms, gate_tx, rsps_rx)
    }
}

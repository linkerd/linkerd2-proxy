// A `NewService` called `NewClassifyGate` that builds a
// `BroadcastClassification` and a `Gate` so that the `gate::Tx` can be
// controlled by the `BroadcastClassification`'s `Tx`.

use crate::classify::{BroadcastClassification, ClassifyResponse};
use linkerd_stack::{gate, layer, ExtractParam, Gate, NewService};
use std::marker::PhantomData;
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub struct Params<C> {
    pub responses: mpsc::Sender<C>,
    pub gate: gate::Rx,
}

pub struct NewClassifyGateSet<P, C, X, N> {
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

impl<P, C, X: Clone, N> NewClassifyGateSet<P, C, X, N> {
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

impl<P, C, N> NewClassifyGateSet<P, C, (), N> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<T, P, C, X, N> NewService<T> for NewClassifyGateSet<P, C, X, N>
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

use crate::classify;
use linkerd_stack::{gate, NewService};
use std::marker::PhantomData;
use tokio::sync::mpsc;

#[derive(Debug)]
pub struct Handle<T> {
    pub gate: gate::Tx,
    pub responses: mpsc::Receiver<T>,
}

#[derive(Clone, Debug)]
pub struct NewFailureGate<Cls, ClsP, X, N> {
    gate: gate::Rx,
    tx: mpsc::Sender<Cls>,
    inner: N,
    extract: X,
    _marker: PhantomData<fn() -> ClsP>,
}

impl<Cls, ClsP, X, N> NewFailureGate<Cls, ClsP, X, N> {
    pub fn channel(extract: X, capacity: usize, inner: N) -> (Self, Handle<T>) {
        let (gate, rx) = gate::channel();
        let (tx, responses) = mpsc::channel(capacity);
        let ch = Handle { gate, responses };
        (Self::new(extract, rx, tx, inner), ch)
    }

    pub fn new(extract: X, gate: gate::Rx, tx: mpsc::Sender<Cls>, inner: N) -> Self {
        Self {
            extract,
            gate,
            tx,
            inner,
            _marker: PhantomData,
        }
    }
}

impl<Cls, T, X, N> NewService<T> for NewFailureGate<Cls::Class, Cls, X, N>
where
    Cls: classify::ClassifyResponse + Default,
    N: NewService<T>,
{
    type Service = ();

    fn new_service(&self, target: T) -> Self::Service {
        let inner = self.inner.new_service(target);
    }
}

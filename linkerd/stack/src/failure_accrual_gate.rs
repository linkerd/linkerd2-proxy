use crate::{gate, NewService};
use tokio::sync::mpsc;

#[derive(Debug)]
pub struct Channel<T> {
    pub gate: gate::Tx,
    pub responses: mpsc::Receiver<T>,
}

#[derive(Clone, Debug)]
pub struct NewFailureAccrualGate<T, N> {
    gate: gate::Rx,
    tx: mpsc::Sender<T>,
    inner: N,
}

impl<T, N> NewFailureAccrualGate<T, N> {
    pub fn channel(capacity: usize, inner: N) -> (Self, Channel<T>) {
        let (gate, rx) = gate::channel();
        let (tx, responses) = mpsc::channel(capacity);
        let ch = Channel { gate, responses };
        (Self::new(rx, tx, inner), ch)
    }

    pub fn new(gate: gate::Rx, tx: mpsc::Sender<T>, inner: N) -> Self {
        Self { gate, tx, inner }
    }
}

impl<T, N> NewService<T> for NewFailureAccrualGate<T, N> {
    type Service = FailureAccrualGate<T, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let inner = self.inner.new_service(target);
        FailureAccrualGate {
            gate: self.gate.clone(),
            tx: self.tx.clone(),
            inner,
        }
    }
}

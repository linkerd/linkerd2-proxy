use crate::classify;
use linkerd_stack::{gate, NewService};
use std::marker::PhantomData;
use tokio::sync::mpsc;

pub struct FailureGate<S> {
    inner: S,
    rx: mpsc::Sender<()>,
}

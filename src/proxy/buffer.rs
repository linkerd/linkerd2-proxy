extern crate linkerd2_router as rt;

use std::marker::PhantomData;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use std::{error, fmt};

use futures::{Async, Future, Poll};
use tokio_timer::{clock, Delay};
use tower::buffer;

use logging;
use proxy::Error;
use svc;

pub trait ExtractDeadline<Req>: Clone {
    fn extract(&self, req: &Req) -> Option<Instant>;
}

/// Produces `MakeService`s where the output `Service` is wrapped with a `Buffer`
#[derive(Debug)]
pub struct Layer<D, Req> {
    capacity: usize,
    deadline: D,
    _marker: PhantomData<fn(Req)>,
}

/// Produces `MakeService`s where the output `Service` is wrapped with a `Buffer`
#[derive(Debug)]
pub struct Make<M, D, Req> {
    capacity: usize,
    deadline: D,
    inner: M,
    _marker: PhantomData<fn(Req)>,
}

type Holder<Req> = Arc<Mutex<Option<Req>>>;
type Stealer<Req> = Weak<Mutex<Option<Req>>>;

pub struct Enqueue<S, D, Req>
where
    S: svc::Service<Req>,
    S::Error: Into<Error>,
{
    deadline: D,
    inner: buffer::Buffer<Dequeue<S>, Stealer<Req>>,
}

pub struct Dequeue<S>(S);

pub struct EnqueueFuture<F, Req> {
    holder: Holder<Req>,
    inner: buffer::future::ResponseFuture<DequeueFuture<F>>,
    timeout: Option<Delay>,
}

pub enum DequeueFuture<F> {
    Lost,
    Inner(F),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Aborted;

pub struct MakeFuture<F, T, D, Req> {
    capacity: usize,
    deadline: D,
    executor: logging::ContextualExecutor<T>,
    inner: F,
    _marker: PhantomData<fn(Req)>,
}

// === impl Layer ===

pub fn layer<D, Req>(capacity: usize, deadline: D) -> Layer<D, Req>
where
    D: ExtractDeadline<Req>,
    Req: Send + 'static,
{
    Layer {
        capacity,
        deadline,
        _marker: PhantomData,
    }
}

impl<D: Clone, Req> Clone for Layer<D, Req> {
    fn clone(&self) -> Self {
        Self {
            capacity: self.capacity,
            deadline: self.deadline.clone(),
            _marker: PhantomData,
        }
    }
}

impl<M, D, Req> svc::Layer<M> for Layer<D, Req>
where
    D: ExtractDeadline<Req>,
{
    type Service = Make<M, D, Req>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service {
            capacity: self.capacity,
            deadline: self.deadline.clone(),
            inner,
            _marker: PhantomData,
        }
    }
}

// === impl Make ===

impl<M: Clone, D: Clone, Req> Clone for Make<M, D, Req> {
    fn clone(&self) -> Self {
        Self {
            capacity: self.capacity,
            deadline: self.deadline.clone(),
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T, M, D, Req> svc::Service<T> for Make<M, D, Req>
where
    T: fmt::Display + Clone + Send + Sync + 'static,
    M: svc::Service<T>,
    M::Response: svc::Service<Req> + Send + 'static,
    M::Error: Into<Error>,
    <M::Response as svc::Service<Req>>::Future: Send,
    <M::Response as svc::Service<Req>>::Error: Into<Error>,
    D: ExtractDeadline<Req>,
    Req: Send + 'static,
{
    type Response = Enqueue<M::Response, D, Req>;
    type Error = Error;
    type Future = MakeFuture<M::Future, T, D, Req>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let executor = logging::context_executor(target.clone());
        let inner = self.inner.call(target);

        Self::Future {
            capacity: self.capacity,
            deadline: self.deadline.clone(),
            executor,
            inner,
            _marker: PhantomData,
        }
    }
}

impl<T, M, D, Req> rt::Make<T> for Make<M, D, Req>
where
    T: fmt::Display + Clone + Send + Sync + 'static,
    M: rt::Make<T>,
    M::Value: svc::Service<Req> + Send + 'static,
    <M::Value as svc::Service<Req>>::Future: Send,
    <M::Value as svc::Service<Req>>::Error: Into<Error>,
    D: ExtractDeadline<Req>,
    Req: Send + 'static,
{
    type Value = Enqueue<M::Value, D, Req>;

    fn make(&self, target: &T) -> Self::Value {
        Enqueue::new(
            self.inner.make(target),
            self.deadline.clone(),
            self.capacity,
            &mut logging::context_executor(target.clone()),
        )
    }
}

impl<M, D, Req> Make<M, D, Req> {
    /// Creates a buffer immediately.
    pub fn make<T>(&self, target: T) -> Enqueue<M::Value, D, Req>
    where
        T: fmt::Display + Clone + Send + Sync + 'static,
        M: rt::Make<T>,
        M::Value: svc::Service<Req> + Send + 'static,
        <M::Value as svc::Service<Req>>::Future: Send,
        <M::Value as svc::Service<Req>>::Error: Into<Error>,
        Req: Send + 'static,
        D: ExtractDeadline<Req> + Clone,
    {
        Enqueue::new(
            self.inner.make(&target),
            self.deadline.clone(),
            self.capacity,
            &mut logging::context_executor(target),
        )
    }
}

// === impl MakeFuture ===

impl<F, T, D, Req, Svc> Future for MakeFuture<F, T, D, Req>
where
    F: Future<Item = Svc>,
    F::Error: Into<Error>,
    Svc: svc::Service<Req> + Send + 'static,
    Svc::Future: Send,
    Svc::Error: Into<Error>,
    D: ExtractDeadline<Req>,
    Req: Send + 'static,
    T: fmt::Display + Send + Sync + 'static,
{
    type Item = Enqueue<Svc, D, Req>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let svc = try_ready!(self.inner.poll().map_err(Into::into));
        let enq = Enqueue::new(
            svc,
            self.deadline.clone(),
            self.capacity,
            &mut self.executor,
        );
        Ok(enq.into())
    }
}

// === impl Enqueue ===

impl<S, D, Req> Enqueue<S, D, Req>
where
    S: svc::Service<Req> + Send + 'static,
    S::Error: Into<Error>,
    S::Future: Send,
    Dequeue<S>: svc::Service<Stealer<Req>, Error = Error> + Send,
    <Dequeue<S> as svc::Service<Stealer<Req>>>::Error: Into<Error>,
    <Dequeue<S> as svc::Service<Stealer<Req>>>::Future: Send,
    D: ExtractDeadline<Req>,
    Req: Send + 'static,
{
    pub fn new<E>(svc: S, deadline: D, capacity: usize, exec: &mut E) -> Self
    where
        E: buffer::WorkerExecutor<Dequeue<S>, Stealer<Req>>,
    {
        let inner = buffer::Buffer::with_executor(Dequeue(svc), capacity, exec);
        Self { deadline, inner }
    }
}

impl<S, D, Req> svc::Service<Req> for Enqueue<S, D, Req>
where
    Req: Send + 'static,
    S: svc::Service<Req>,
    S::Error: Into<Error>,
    S::Future: Send,
    D: ExtractDeadline<Req>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = EnqueueFuture<S::Future, Req>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let timeout = self.deadline.extract(&req).map(Delay::new);
        let holder = Arc::new(Mutex::new(Some(req)));
        let stealer = Arc::downgrade(&holder);

        EnqueueFuture {
            holder,
            timeout,
            inner: self.inner.call(stealer),
        }
    }
}

impl<S, D, Req> Clone for Enqueue<S, D, Req>
where
    S: svc::Service<Req>,
    S::Error: Into<Error>,
    D: Clone,
{
    fn clone(&self) -> Self {
        Self {
            deadline: self.deadline.clone(),
            inner: self.inner.clone(),
        }
    }
}

// === impl EnqueueFuture ===

impl<Req, F> Future for EnqueueFuture<F, Req>
where
    F: Future,
    F::Error: Into<Error>,
{
    type Item = F::Item;
    type Error = Error;

    fn poll(&mut self) -> Poll<F::Item, Self::Error> {
        if let Async::Ready(v) = self.inner.poll()? {
            return Ok(Async::Ready(v));
        }

        // If the request hasn't been consumed by `Dequeue`, then steal it and
        // drop it when the timeout fires.
        if let Ok(mut h) = self.holder.lock() {
            if h.is_some() {
                if let Some(t) = self.timeout.as_mut() {
                    if t.poll().map_err(Error::from)?.is_ready() {
                        drop(h.take());
                        return Err(Aborted.into());
                    }
                }
            }
        }

        return Ok(Async::NotReady);
    }
}

// === impl Dequeue ===

impl<S, Req> svc::Service<Stealer<Req>> for Dequeue<S>
where
    S: svc::Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = DequeueFuture<S::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, req: Stealer<Req>) -> Self::Future {
        match req.upgrade() {
            None => DequeueFuture::Lost,
            Some(l) => match l.lock() {
                Err(_) => DequeueFuture::Lost,
                Ok(ref mut v) => match v.take() {
                    None => DequeueFuture::Lost,
                    Some(req) => DequeueFuture::Inner(self.0.call(req)),
                },
            },
        }
    }
}

// === impl DequeueFuture ===

impl<F> Future for DequeueFuture<F>
where
    F: Future,
    F::Error: Into<Error>,
{
    type Item = F::Item;
    type Error = Error;

    fn poll(&mut self) -> Poll<F::Item, Self::Error> {
        match self {
            DequeueFuture::Lost => Err(Aborted.into()),
            DequeueFuture::Inner(ref mut f) => f.poll().map_err(Into::into),
        }
    }
}

// === Aborted ===

impl fmt::Display for Aborted {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "the request could not be dispatched in a timely fashion")
    }
}

impl error::Error for Aborted {}

// === impl ExtractDeadline ===

impl<Req> ExtractDeadline<Req> for () {
    fn extract(&self, _: &Req) -> Option<Instant> {
        None
    }
}

impl<F, Req> ExtractDeadline<Req> for F
where
    F: Fn(&Req) -> Option<Instant>,
    F: Clone,
{
    fn extract(&self, req: &Req) -> Option<Instant> {
        (self)(req)
    }
}

impl<Req> ExtractDeadline<Req> for Duration {
    fn extract(&self, _: &Req) -> Option<Instant> {
        Some(clock::now() + *self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future;
    use futures::sync::oneshot::{self, Receiver, Sender};
    use svc::Service;

    struct Idle;
    impl svc::Service<()> for Idle {
        type Response = ();
        type Error = Error;
        type Future = future::Empty<(), Error>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            Ok(Async::NotReady)
        }

        fn call(&mut self, _: ()) -> Self::Future {
            unimplemented!("what you do?!")
        }
    }

    struct Active(Option<Sender<Sender<()>>>);
    impl svc::Service<()> for Active {
        type Response = ();
        type Error = Error;
        type Future = Rsp;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            Ok(Async::Ready(()))
        }

        fn call(&mut self, _: ()) -> Self::Future {
            let (tx, rx) = oneshot::channel();
            self.0
                .take()
                .expect("lost sender")
                .send(tx)
                .expect("rx must be alive");
            Rsp(rx)
        }
    }

    struct Rsp(Receiver<()>);
    impl Future for Rsp {
        type Item = ();
        type Error = Error;
        fn poll(&mut self) -> Poll<(), Error> {
            Ok(self.0.poll().expect("must not fail"))
        }
    }

    #[test]
    fn request_aborted_with_idle_service() {
        tokio::run(future::lazy(|| {
            let mut svc = Enqueue::new(
                Idle,
                Duration::from_millis(100),
                1,
                &mut logging::context_executor("test"),
            );

            svc.poll_ready().expect("service must be ready");

            svc.call(()).then(|r| match r {
                Ok(_) => panic!("unexpected response from idle service"),
                Err(e) => {
                    assert!(e.downcast::<Aborted>().is_ok());
                    future::ok(())
                }
            })
        }));
    }

    #[test]
    fn request_not_aborted_if_dispatched() {
        tokio::run(future::lazy(|| {
            let (tx, rx) = oneshot::channel();
            let mut svc = Enqueue::new(
                Active(Some(tx)),
                Duration::from_millis(100),
                1,
                &mut logging::context_executor("test"),
            );

            svc.poll_ready().expect("service must be ready");

            let call = svc.call(());
            rx.map_err(|_| ()).and_then(move |rsp_tx| {
                rsp_tx.send(()).expect("service lost");
                call.map_err(|_| ())
            })
        }));
    }
}

use crate::svc;
use futures::{Async, Future, Poll};
use linkerd2_error::Error;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use std::{error, fmt};
use tokio_timer::{clock, Delay};
use tower::buffer;
use tracing_futures::Instrument;

/// Determines the dispatch deadline for a request.
pub trait Deadline<Req>: Clone {
    fn deadline(&self, req: &Req) -> Option<Instant>;
}

/// Produces `MakeService`s where the output `Service` is wrapped with a `Buffer`
#[derive(Debug)]
pub struct Layer<D, Req> {
    capacity: usize,
    deadline: D,
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

// === impl Layer ===

pub fn layer<D, Req>(capacity: usize, deadline: D) -> Layer<D, Req>
where
    D: Deadline<Req>,
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

impl<S, D, Req> svc::Layer<S> for Layer<D, Req>
where
    D: Deadline<Req>,
    S: svc::Service<Req> + Send + 'static,
    S::Error: Into<Error>,
    S::Future: Send,
    Req: Send + 'static,
{
    type Service = Enqueue<S, D, Req>;

    fn layer(&self, inner: S) -> Self::Service {
        Enqueue::new(inner, self.deadline.clone(), self.capacity)
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
    D: Deadline<Req>,
    Req: Send + 'static,
{
    fn new(svc: S, deadline: D, capacity: usize) -> Self {
        let mut exec = tokio::executor::DefaultExecutor::current().in_current_span();
        let inner = buffer::Buffer::with_executor(Dequeue(svc), capacity, &mut exec);
        Self { deadline, inner }
    }
}

impl<S, D, Req> svc::Service<Req> for Enqueue<S, D, Req>
where
    Req: Send + 'static,
    S: svc::Service<Req>,
    S::Error: Into<Error>,
    S::Future: Send,
    D: Deadline<Req>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = EnqueueFuture<S::Future, Req>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let timeout = self.deadline.deadline(&req).map(Delay::new);
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
        let mut h = self.holder.lock().expect("inner service panicked");
        if h.is_some() {
            if let Some(t) = self.timeout.as_mut() {
                if t.poll().map_err(Error::from)?.is_ready() {
                    drop(h.take());
                    return Err(Aborted.into());
                }
            }
        } else {
            // Drop the timeout future so the timer doesn't need to track it.
            drop(self.timeout.take());
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
        req.upgrade()
            .and_then(|l| l.lock().ok()?.take())
            .map(|req| DequeueFuture::Inner(self.0.call(req)))
            .unwrap_or(DequeueFuture::Lost)
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
            DequeueFuture::Inner(ref mut f) => f.poll().map_err(Into::into),
            DequeueFuture::Lost => Err(Aborted.into()),
        }
    }
}

// === Aborted ===

impl fmt::Display for Aborted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the request could not be dispatched in a timely fashion")
    }
}

impl error::Error for Aborted {}

// === impl Deadline ===

impl<Req> Deadline<Req> for () {
    fn deadline(&self, _: &Req) -> Option<Instant> {
        None
    }
}

impl<F, Req> Deadline<Req> for F
where
    F: Fn(&Req) -> Option<Instant>,
    F: Clone,
{
    fn deadline(&self, req: &Req) -> Option<Instant> {
        (self)(req)
    }
}

impl<Req> Deadline<Req> for Duration {
    fn deadline(&self, _: &Req) -> Option<Instant> {
        Some(clock::now() + *self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future;
    use futures::sync::oneshot::{self, Receiver, Sender};
    use svc::Service;

    struct Idle(Arc<()>);
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
            let mut svc = Enqueue::new(Idle(Arc::new(())), Duration::from_millis(100), 1);

            assert!(svc.poll_ready().ok().map(|r| r.is_ready()).unwrap_or(false));

            svc.call(()).then(|r| match r {
                Ok(_) => panic!("unexpected response from idle service"),
                Err(e) => {
                    e.downcast::<Aborted>().expect("request must be aborted");
                    future::ok(())
                }
            })
        }));
    }

    #[test]
    fn inner_service_dropped() {
        tokio::run(future::lazy(|| {
            let anchor = Arc::new(());
            let handle = Arc::downgrade(&anchor);
            let inner = Idle(anchor);
            let mut svc = Enqueue::new(inner, Duration::from_secs(0), 1);

            assert!(svc.poll_ready().ok().map(|r| r.is_ready()).unwrap_or(false));
            let call = svc.call(());
            drop(svc);

            tokio::timer::Timeout::new(call, Duration::from_millis(100)).then(move |r| match r {
                Ok(()) => panic!("unexpected response from idle service"),
                Err(_) => {
                    assert!(
                        handle.upgrade().is_none(),
                        "inner service must have been dropped",
                    );
                    future::ok(())
                }
            })
        }));
    }

    #[test]
    fn request_not_aborted_if_dispatched() {
        tokio::run(future::lazy(|| {
            let (tx, rx) = oneshot::channel();
            let mut svc = Enqueue::new(Active(Some(tx)), Duration::from_millis(100), 1);

            svc.poll_ready().expect("service must be ready");

            let call = svc.call(());
            rx.map_err(|_| ()).and_then(move |rsp_tx| {
                rsp_tx.send(()).expect("service lost");
                call.map_err(|_| ())
            })
        }));
    }
}

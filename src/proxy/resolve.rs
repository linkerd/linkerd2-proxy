use crate::svc;
use crate::Error;
use futures::{stream::FuturesUnordered, try_ready, Async, Future, Poll, Stream};
use indexmap::IndexMap;
use std::{fmt, net::SocketAddr};
use tokio::sync::oneshot;
pub use tower_discover::Change;
use tracing::trace;

/// Resolves `T`-typed names/addresses as a `Resolution`.
pub trait Resolve<T> {
    type Endpoint;
    type Resolution: Resolution<Endpoint = Self::Endpoint>;
    type Future: Future<Item = Self::Resolution>;

    /// Asynchronously returns a `Resolution` for the given `target`.
    ///
    /// The returned future will complete with a `Resolution` if this resolver
    /// was able to successfully resolve `target`. Otherwise, if it completes
    /// with an error, that name or address should not be resolved by this
    /// resolver.
    fn resolve(&self, target: &T) -> Self::Future;
}

/// An infinite stream of endpoint updates.
pub trait Resolution {
    type Endpoint;
    type Error;

    fn poll(&mut self) -> Poll<Update<Self::Endpoint>, Self::Error>;
}

#[derive(Clone, Debug)]
pub enum Update<T> {
    Add(SocketAddr, T),
    Remove(SocketAddr),
}

#[derive(Clone, Debug)]
pub struct Layer<R> {
    resolve: R,
}

#[derive(Clone, Debug)]
pub struct MakeSvc<R, M> {
    resolve: R,
    inner: M,
}

/// Observes an `R`-typed resolution stream, using an `M`-typed endpoint stack to
/// build a service for each endpoint.
pub struct Discover<R: Resolution, M: svc::Service<R::Endpoint>> {
    resolution: R,
    make: M,
    make_futures: MakeFutures<M::Future>,
}

pub struct DiscoverFuture<F, M> {
    future: F,
    make: M,
}

struct MakeFutures<F> {
    futures: FuturesUnordered<MakeFuture<F>>,
    cancelations: IndexMap<SocketAddr, oneshot::Sender<()>>,
}

struct MakeFuture<F> {
    inner: F,
    canceled: oneshot::Receiver<()>,
    addr: SocketAddr,
}

enum MakeError<E> {
    Inner(E),
    Canceled,
}

// === impl Layer ===

pub fn layer<T, R>(resolve: R) -> Layer<R>
where
    R: Resolve<T> + Clone,
    R::Endpoint: fmt::Debug,
{
    Layer { resolve }
}

impl<R, M> svc::Layer<M> for Layer<R>
where
    R: Clone,
{
    type Service = MakeSvc<R, M>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeSvc {
            resolve: self.resolve.clone(),
            inner,
        }
    }
}

// === impl MakeSvc ===

impl<T, R, M> svc::Service<T> for MakeSvc<R, M>
where
    R: Resolve<T>,
    R::Endpoint: fmt::Debug,
    M: svc::Service<R::Endpoint> + Clone,
{
    type Response = Discover<R::Resolution, M>;
    type Error = <R::Future as Future>::Error;
    type Future = DiscoverFuture<R::Future, Option<M>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into()) // always ready to make a Discover
    }

    fn call(&mut self, target: T) -> Self::Future {
        let future = self.resolve.resolve(&target);
        DiscoverFuture {
            future,
            make: Some(self.inner.clone()),
        }
    }
}

// === impl DiscoverFuture ===

impl<F, M> Future for DiscoverFuture<F, Option<M>>
where
    F: Future,
    F::Item: Resolution,
    M: svc::Service<<F::Item as Resolution>::Endpoint>,
{
    type Item = Discover<F::Item, M>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let resolution = try_ready!(self.future.poll());
        let make = self.make.take().expect("polled after ready");
        Ok(Async::Ready(Discover::new(resolution, make)))
    }
}

// === impl Discover ===

impl<R, M> Discover<R, M>
where
    R: Resolution,
    M: svc::Service<R::Endpoint>,
{
    fn new(resolution: R, make: M) -> Self {
        Self {
            resolution,
            make,
            make_futures: MakeFutures::new(),
        }
    }
}

impl<R, M> Discover<R, M>
where
    R: Resolution,
    R::Endpoint: fmt::Debug,
    R::Error: Into<Error>,
    M: svc::Service<R::Endpoint>,
    M::Error: Into<Error>,
{
    fn poll_resolution(&mut self) -> Poll<Change<SocketAddr, M::Response>, Error> {
        loop {
            // Before polling the resolution, where we could potentially receive
            // an `Add`, poll_ready to ensure that `make` is ready to build new
            // services. Don't process any updates until we can do so.
            try_ready!(self.make.poll_ready().map_err(Into::into));

            let update = try_ready!(self.resolution.poll().map_err(Into::into));
            trace!("watch: {:?}", update);
            match update {
                Update::Add(addr, target) => {
                    // Start building the service and continue. If a pending
                    // service exists for this addr, it will be canceled.
                    let fut = self.make.call(target);
                    self.make_futures.push(addr, fut);
                }
                Update::Remove(addr) => {
                    self.make_futures.remove(&addr);
                    return Ok(Async::Ready(Change::Remove(addr)));
                }
            }
        }
    }
}

impl<R, M> tower_discover::Discover for Discover<R, M>
where
    R: Resolution,
    R::Endpoint: fmt::Debug,
    R::Error: Into<Error>,
    M: svc::Service<R::Endpoint>,
    M::Error: Into<Error>,
{
    type Key = SocketAddr;
    type Service = M::Response;
    type Error = Error;

    fn poll(&mut self) -> Poll<Change<Self::Key, Self::Service>, Self::Error> {
        if let Async::Ready(change) = self.poll_resolution()? {
            return Ok(Async::Ready(change));
        }

        if let Async::Ready(Some((addr, svc))) = self.make_futures.poll().map_err(Into::into)? {
            return Ok(Async::Ready(Change::Insert(addr, svc)));
        }

        Ok(Async::NotReady)
    }
}

// === impl MakeFutures ===

impl<F: Future> MakeFutures<F> {
    fn new() -> Self {
        Self {
            futures: FuturesUnordered::new(),
            cancelations: IndexMap::new(),
        }
    }

    fn push(&mut self, addr: SocketAddr, inner: F) {
        let (cancel, canceled) = oneshot::channel();
        if let Some(prior) = self.cancelations.insert(addr, cancel) {
            let _ = prior.send(());
        }
        self.futures.push(MakeFuture {
            addr,
            inner,
            canceled,
        });
    }

    fn remove(&mut self, addr: &SocketAddr) {
        if let Some(cancel) = self.cancelations.remove(addr) {
            let _ = cancel.send(());
        }
    }
}

impl<F: Future> Stream for MakeFutures<F> {
    type Item = (SocketAddr, F::Item);
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            return match self.futures.poll() {
                Err(MakeError::Canceled) => continue,
                Err(MakeError::Inner(err)) => Err(err),
                Ok(Async::Ready(Some((addr, svc)))) => {
                    let _rm = self.cancelations.remove(&addr);
                    debug_assert!(_rm.is_some(), "cancelation missing for {}", addr);
                    Ok(Async::Ready(Some((addr, svc))))
                }
                Ok(r) => Ok(r),
            };
        }
    }
}

// === impl MakeFuture ===

impl<F: Future> Future for MakeFuture<F> {
    type Item = (SocketAddr, F::Item);
    type Error = MakeError<F::Error>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Ok(Async::Ready(())) = self.canceled.poll() {
            trace!("canceled making service for {:?}", self.addr);
            return Err(MakeError::Canceled);
        }
        let svc = try_ready!(self.inner.poll());
        Ok((self.addr, svc).into())
    }
}

// === impl MakeError ===

impl<E> From<E> for MakeError<E> {
    fn from(inner: E) -> Self {
        MakeError::Inner(inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future;
    use svc::Service;
    use tokio::sync::mpsc;
    use tower_discover::{Change, Discover as _Discover};
    use tower_util::service_fn;

    impl<E> Resolution for mpsc::Receiver<Update<E>> {
        type Endpoint = E;
        type Error = mpsc::error::RecvError;

        fn poll(&mut self) -> Poll<Update<Self::Endpoint>, Self::Error> {
            let ep = try_ready!(Stream::poll(self)).expect("stream must not terminate");
            Ok(Async::Ready(ep))
        }
    }

    #[derive(Debug)]
    struct Svc<T>(Vec<oneshot::Receiver<T>>);
    impl<T> Service<()> for Svc<T> {
        type Response = T;
        type Error = oneshot::error::RecvError;
        type Future = oneshot::Receiver<T>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            Ok(().into())
        }

        fn call(&mut self, _: ()) -> Self::Future {
            self.0.pop().expect("exhausted")
        }
    }

    #[test]
    fn inserts_delivered_out_of_order() {
        with_task(move || {
            let (mut reso_tx, resolution) = mpsc::channel(2);
            let (make0_tx, make0_rx) = oneshot::channel::<Svc<usize>>();
            let (make1_tx, make1_rx) = oneshot::channel::<Svc<usize>>();
            let make = Svc(vec![make1_rx, make0_rx]);

            let mut discover = Discover::new(resolution, make);
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without updates"
            );

            let addr0 = SocketAddr::from(([127, 0, 0, 1], 80));
            reso_tx.try_send(Update::Add(addr0, ())).unwrap();
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without service being made"
            );
            assert_eq!(
                discover.make_futures.futures.len(),
                1,
                "must be only one pending make"
            );
            assert_eq!(
                discover.make_futures.cancelations.len(),
                1,
                "no pending cancelation"
            );

            let addr1 = SocketAddr::from(([127, 0, 0, 2], 80));
            reso_tx
                .try_send(Update::Add(addr1, ()))
                .expect("update must be sent");
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without service being made"
            );
            assert_eq!(
                discover.make_futures.futures.len(),
                2,
                "must be only one pending make"
            );
            assert_eq!(
                discover.make_futures.cancelations.len(),
                2,
                "no pending cancelation"
            );

            let (rsp1_tx, rsp1_rx) = oneshot::channel();
            make1_tx
                .send(Svc(vec![rsp1_rx]))
                .expect("make must receive service");
            match discover.poll().expect("discover can't fail") {
                Async::NotReady => panic!("not processed"),
                Async::Ready(Change::Remove(..)) => panic!("unexpected remove"),
                Async::Ready(Change::Insert(a, mut svc)) => {
                    assert_eq!(a, addr1);

                    assert!(svc.poll_ready().unwrap().is_ready());
                    let mut fut = svc.call(());
                    assert!(fut.poll().unwrap().is_not_ready());
                    rsp1_tx.send(1).unwrap();
                    assert_eq!(fut.poll().unwrap(), Async::Ready(1));
                }
            }
            assert_eq!(
                discover.make_futures.futures.len(),
                1,
                "must be only one pending make"
            );
            assert_eq!(
                discover.make_futures.cancelations.len(),
                1,
                "no pending cancelation"
            );

            let (rsp0_tx, rsp0_rx) = oneshot::channel();
            make0_tx
                .send(Svc(vec![rsp0_rx]))
                .expect("make must receive service");
            match discover.poll().expect("discover can't fail") {
                Async::NotReady => panic!("not processed"),
                Async::Ready(Change::Remove(..)) => panic!("unexpected remove"),
                Async::Ready(Change::Insert(a, mut svc)) => {
                    assert_eq!(a, addr0);

                    assert!(svc.poll_ready().unwrap().is_ready());
                    let mut fut = svc.call(());
                    assert!(fut.poll().unwrap().is_not_ready());
                    rsp0_tx.send(0).unwrap();
                    assert_eq!(fut.poll().unwrap(), Async::Ready(0));
                }
            }
            assert!(discover.make_futures.futures.is_empty(), "futures remain");
            assert!(
                discover.make_futures.cancelations.is_empty(),
                "cancelation remains"
            );
        });
    }

    #[test]
    fn overwriting_insert_cancels_original() {
        with_task(move || {
            let (mut reso_tx, resolution) = mpsc::channel(2);
            let (make0_tx, make0_rx) = oneshot::channel::<Svc<usize>>();
            let (make1_tx, make1_rx) = oneshot::channel::<Svc<usize>>();
            let make = Svc(vec![make1_rx, make0_rx]);

            let mut discover = Discover::new(resolution, make);
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without updates"
            );

            let addr = SocketAddr::from(([127, 0, 0, 1], 80));
            reso_tx.try_send(Update::Add(addr, ())).unwrap();
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without service being made"
            );
            assert_eq!(
                discover.make_futures.futures.len(),
                1,
                "must be only one pending make"
            );
            assert_eq!(
                discover.make_futures.cancelations.len(),
                1,
                "no pending cancelation"
            );

            reso_tx
                .try_send(Update::Add(addr, ()))
                .expect("update must be sent");
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without service being made"
            );
            assert_eq!(
                discover.make_futures.futures.len(),
                1,
                "must be only one pending make"
            );
            assert_eq!(
                discover.make_futures.cancelations.len(),
                1,
                "no pending cancelation"
            );

            make0_tx
                .send(Svc(vec![]))
                .expect_err("receiver must have been dropped");

            let (rsp1_tx, rsp1_rx) = oneshot::channel();
            make1_tx
                .send(Svc(vec![rsp1_rx]))
                .expect("make must receive service");
            match discover.poll().expect("discover can't fail") {
                Async::NotReady => panic!("not processed"),
                Async::Ready(Change::Remove(..)) => panic!("unexpected remove"),
                Async::Ready(Change::Insert(a, mut svc)) => {
                    assert_eq!(a, addr);

                    assert!(svc.poll_ready().unwrap().is_ready());
                    let mut fut = svc.call(());
                    assert!(fut.poll().unwrap().is_not_ready());
                    rsp1_tx.send(1).unwrap();
                    assert_eq!(fut.poll().unwrap(), Async::Ready(1));
                }
            }
            assert!(
                discover.make_futures.cancelations.is_empty(),
                "cancelation remains"
            );
        });
    }

    #[test]
    fn cancelation_of_pending_service() {
        with_task(move || {
            let (mut tx, resolution) = mpsc::channel(1);
            let make = service_fn(|()| future::empty::<Svc<()>, Error>());

            let mut discover = Discover::new(resolution, make);
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without updates"
            );

            let addr = SocketAddr::from(([127, 0, 0, 1], 80));
            tx.try_send(Update::Add(addr, ())).unwrap();
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without service being made"
            );
            assert_eq!(
                discover.make_futures.cancelations.len(),
                1,
                "no pending cancelation"
            );

            tx.try_send(Update::Remove(addr)).unwrap();
            match discover.poll().expect("discover can't fail") {
                Async::NotReady => panic!("remove not processed"),
                Async::Ready(Change::Insert(..)) => panic!("unexpected insert"),
                Async::Ready(Change::Remove(a)) => assert_eq!(a, addr),
            }
            assert!(
                discover.make_futures.cancelations.is_empty(),
                "cancelation remains"
            );
        });
    }

    fn with_task<F: FnOnce() -> U, U>(f: F) -> U {
        future::lazy(|| Ok::<_, ()>(f())).wait().unwrap()
    }
}

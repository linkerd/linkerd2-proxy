use futures::{stream::FuturesUnordered, try_ready, Async, Future, Poll, Stream};
use indexmap::IndexMap;
use linkerd2_proxy_core::Error;
use std::fmt;
use std::net::SocketAddr;
use tokio::sync::oneshot;
use tower::discover::{self, Change};

#[derive(Clone, Debug)]
pub struct MakeEndpoint<D, E> {
    make_discover: D,
    make_endpoint: E,
}

#[derive(Debug)]
pub struct DiscoverFuture<F, M> {
    future: F,
    make_endpoint: Option<M>,
}

/// Observes an `R`-typed resolution stream, using an `M`-typed endpoint stack to
/// build a service for each endpoint.
pub struct Discover<D: discover::Discover, E: tower::Service<D::Service>> {
    discover: D,
    make_endpoint: E,
    make_futures: MakeFutures<E::Future>,
    pending_removals: Vec<SocketAddr>,
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

// === impl MakeEndpoint ===

impl<D, E> MakeEndpoint<D, E> {
    pub fn new<T, Endpoint>(make_endpoint: E, make_discover: D) -> Self
    where
        D: tower::Service<T> + Clone,
        D::Response: discover::Discover<Key = SocketAddr, Service = Endpoint>,
        E: tower::Service<Endpoint> + Clone,
        Endpoint: fmt::Debug + Clone + PartialEq,
    {
        Self {
            make_discover,
            make_endpoint,
        }
    }
}

impl<T, D, E, InnerDiscover> tower::Service<T> for MakeEndpoint<D, E>
where
    D: tower::Service<T, Response = InnerDiscover> + Clone,
    InnerDiscover: discover::Discover<Key = SocketAddr>,
    InnerDiscover::Error: Into<Error>,
    InnerDiscover::Service: fmt::Debug + Clone + PartialEq,
    E: tower::Service<InnerDiscover::Service> + Clone,
    E::Error: Into<Error>,
{
    type Response = Discover<D::Response, E>;
    type Error = D::Error;
    type Future = DiscoverFuture<D::Future, E>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.make_discover.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let future = self.make_discover.call(target);
        DiscoverFuture {
            future,
            make_endpoint: Some(self.make_endpoint.clone()),
        }
    }
}

// === impl DiscoverFuture ===

impl<F, E, D> Future for DiscoverFuture<F, E>
where
    F: Future<Item = D>,
    D: discover::Discover<Key = SocketAddr>,
    D::Error: Into<Error>,
    D::Service: fmt::Debug + Clone + PartialEq,
    E: tower::Service<D::Service>,
    E::Error: Into<Error>,
{
    type Item = Discover<F::Item, E>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let resolution = try_ready!(self.future.poll());
        let make_endpoint = self.make_endpoint.take().expect("polled after ready");
        Ok(Async::Ready(Discover::new(resolution, make_endpoint)))
    }
}

// === impl Discover ===

impl<D, E> Discover<D, E>
where
    D: discover::Discover<Key = SocketAddr>,
    D::Error: Into<Error>,
    D::Service: Clone + PartialEq + std::fmt::Debug,
    E: tower::Service<D::Service>,
    E::Error: Into<Error>,
{
    pub fn new(discover: D, make_endpoint: E) -> Self {
        Self {
            discover,
            make_endpoint,
            make_futures: MakeFutures::new(),
            pending_removals: Vec::new(),
        }
    }
}

impl<D, E> discover::Discover for Discover<D, E>
where
    D: discover::Discover<Key = SocketAddr>,
    D::Error: Into<Error>,
    D::Service: fmt::Debug + Clone + PartialEq,
    E: tower::Service<D::Service> + Clone,
    E::Error: Into<Error>,
{
    type Key = SocketAddr;
    type Service = E::Response;
    type Error = Error;

    fn poll(&mut self) -> Poll<Change<Self::Key, Self::Service>, Self::Error> {
        if let Async::Ready(addr) = self.poll_removals()? {
            return Ok(Async::Ready(Change::Remove(addr)));
        }

        if let Async::Ready(Some((addr, svc))) = self.make_futures.poll().map_err(Into::into)? {
            return Ok(Async::Ready(Change::Insert(addr, svc)));
        }

        Ok(Async::NotReady)
    }
}

impl<D, E> Discover<D, E>
where
    D: discover::Discover<Key = SocketAddr>,
    D::Error: Into<Error>,
    D::Service: Clone + PartialEq + fmt::Debug,
    E: tower::Service<D::Service>,
    E::Error: Into<Error>,
{
    fn poll_removals(&mut self) -> Poll<SocketAddr, Error> {
        loop {
            if let Some(addr) = self.pending_removals.pop() {
                self.make_futures.remove(&addr);
                return Ok(addr.into());
            }

            // Before polling the resolution, where we could potentially receive
            // an `Add`, poll_ready to ensure that `make` is ready to build new
            // services. Don't process any updates until we can do so.
            try_ready!(self.make_endpoint.poll_ready().map_err(Into::into));

            match try_ready!(self.discover.poll().map_err(Into::into)) {
                Change::Insert(addr, target) => {
                    // Start building the service and continue. If a pending
                    // service exists for this addr, it will be canceled.
                    let fut = self.make_endpoint.call(target);
                    self.make_futures.push(addr, fut);
                }
                Change::Remove(addr) => {
                    self.pending_removals.push(addr);
                }
            }
        }
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
            tracing::trace!("canceled making service for {:?}", self.addr);
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
    use tokio::sync::mpsc;
    use tower::discover::{Change, Discover as _Discover};
    use tower::Service;
    use tower_util::service_fn;

    #[derive(Debug)]
    struct Svc<F>(Vec<F>);
    impl<F: Future> Service<()> for Svc<F> {
        type Response = F::Item;
        type Error = F::Error;
        type Future = F;

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
            let (mut reso_tx, reso_rx) = mpsc::channel(2);
            let (make0_tx, make0_rx) = oneshot::channel::<Svc<oneshot::Receiver<usize>>>();
            let (make1_tx, make1_rx) = oneshot::channel::<Svc<oneshot::Receiver<usize>>>();

            let mut discover = Discover::new(reso_rx, Svc(vec![make1_rx, make0_rx]));
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without updates"
            );

            let addr0 = SocketAddr::from(([127, 0, 0, 1], 80));
            reso_tx.try_send(Update::Add(vec![(addr0, ())])).unwrap();
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
                .try_send(Update::Add(vec![(addr1, ())]))
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
            let (mut reso_tx, reso_rx) = mpsc::channel(2);
            let (make0_tx, make0_rx) = oneshot::channel::<Svc<oneshot::Receiver<usize>>>();
            let (make1_tx, make1_rx) = oneshot::channel::<Svc<oneshot::Receiver<usize>>>();

            let mut discover = Discover::new(reso_rx, Svc(vec![make1_rx, make0_rx]));
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without updates"
            );

            let addr = SocketAddr::from(([127, 0, 0, 1], 80));
            reso_tx.try_send(Update::Add(vec![(addr, ())])).unwrap();
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
                .try_send(Update::Add(vec![(addr, ())]))
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
            let (mut tx, reso_rx) = mpsc::channel(1);

            let mut discover =
                Discover::new(reso_rx, service_fn(|()| future::empty::<Svc<()>, Error>()));
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without updates"
            );

            let addr = SocketAddr::from(([127, 0, 0, 1], 80));
            tx.try_send(Update::Add(vec![(addr, ())])).unwrap();
            assert!(
                discover.poll().expect("discover can't fail").is_not_ready(),
                "ready without service being made"
            );
            assert_eq!(
                discover.make_futures.cancelations.len(),
                1,
                "no pending cancelation"
            );

            tx.try_send(Update::Remove(vec![addr])).unwrap();
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

use futures::{ready, stream::FuturesUnordered, Stream, TryFuture};
use indexmap::IndexMap;
use linkerd2_error::Error;
use pin_project::pin_project;
use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::oneshot;
use tower::discover::{self, Change};

#[derive(Clone, Debug)]
pub struct MakeEndpoint<D, E> {
    make_discover: D,
    make_endpoint: E,
}

#[pin_project]
#[derive(Debug)]
pub struct DiscoverFuture<F, M> {
    #[pin]
    future: F,
    make_endpoint: Option<M>,
}

/// Observes an `R`-typed resolution stream, using an `M`-typed endpoint stack to
/// build a service for each endpoint.
#[pin_project]
pub struct Discover<D: discover::Discover, E: tower::Service<D::Service>> {
    #[pin]
    discover: D,
    make_endpoint: E,
    #[pin]
    make_futures: MakeFutures<D::Key, E::Future>,
    pending_removals: Vec<D::Key>,
}

#[pin_project]
struct MakeFutures<K, F> {
    #[pin]
    futures: FuturesUnordered<MakeFuture<K, F>>,
    cancelations: IndexMap<K, oneshot::Sender<()>>,
}

#[pin_project]
struct MakeFuture<K, F> {
    key: Option<K>,
    #[pin]
    inner: F,
    #[pin]
    canceled: oneshot::Receiver<()>,
}

enum MakeError<E> {
    Inner(E),
    Canceled,
}

// === impl MakeEndpoint ===

impl<D, E> MakeEndpoint<D, E> {
    pub fn new(make_endpoint: E, make_discover: D) -> Self {
        Self {
            make_discover,
            make_endpoint,
        }
    }
}

impl<T, D, E, InnerDiscover> tower::Service<T> for MakeEndpoint<D, E>
where
    D: tower::Service<T, Response = InnerDiscover>,
    InnerDiscover: discover::Discover,
    InnerDiscover::Key: Hash + Clone,
    InnerDiscover::Error: Into<Error>,
    E: tower::Service<InnerDiscover::Service> + Clone,
    E::Error: Into<Error>,
{
    type Response = Discover<D::Response, E>;
    type Error = D::Error;
    type Future = DiscoverFuture<D::Future, E>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.make_discover.poll_ready(cx)
    }

    #[inline]
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
    F: TryFuture<Ok = D>,
    D: discover::Discover,
    D::Key: Hash + Clone,
    D::Error: Into<Error>,
    E: tower::Service<D::Service>,
    E::Error: Into<Error>,
{
    type Output = Result<Discover<F::Ok, E>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let resolution = ready!(this.future.try_poll(cx))?;
        let make_endpoint = this.make_endpoint.take().expect("polled after ready");
        Poll::Ready(Ok(Discover::new(resolution, make_endpoint)))
    }
}

// === impl Discover ===

impl<D, E> Discover<D, E>
where
    D: discover::Discover,
    D::Key: Hash + Clone,
    D::Error: Into<Error>,
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

impl<D, E> Stream for Discover<D, E>
where
    D: discover::Discover,
    D::Key: Hash + Clone,
    D::Error: Into<Error>,
    E: tower::Service<D::Service>,
    E::Error: Into<Error>,
{
    type Item = Result<Change<D::Key, E::Response>, Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Change<D::Key, E::Response>, Error>>> {
        if let Poll::Ready(result) = self.poll_removals(cx) {
            if let Some(key) = result? {
                return Poll::Ready(Some(Ok(Change::Remove(key))));
            }
        }

        if let Poll::Ready(Some(res)) = self.project().make_futures.poll_next(cx) {
            let (key, svc) = res.map_err(Into::into)?;
            return Poll::Ready(Some(Ok(Change::Insert(key, svc))));
        }

        Poll::Pending
    }
}

impl<D, E> Discover<D, E>
where
    D: discover::Discover,
    D::Key: Hash + Clone,
    D::Error: Into<Error>,
    E: tower::Service<D::Service>,
    E::Error: Into<Error>,
{
    fn poll_removals(
        self: &mut Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<D::Key>, Error>> {
        loop {
            let mut this = self.as_mut().project();
            if let Some(key) = this.pending_removals.pop() {
                this.make_futures.remove(&key);
                return Poll::Ready(Ok(Some(key)));
            }

            // Before polling the resolution, where we could potentially receive
            // an `Add`, poll_ready to ensure that `make` is ready to build new
            // services. Don't process any updates until we can do so.
            ready!(this.make_endpoint.poll_ready(cx)).map_err(Into::into)?;

            match ready!(this.discover.poll_discover(cx)) {
                Some(change) => match change.map_err(Into::into)? {
                    Change::Insert(key, target) => {
                        // Start building the service and continue. If a pending
                        // service exists for this addr, it will be canceled.
                        let fut = this.make_endpoint.call(target);
                        this.make_futures.push(key, fut);
                    }
                    Change::Remove(key) => {
                        this.pending_removals.push(key);
                    }
                },

                None => return Poll::Ready(Ok(None)),
            }
        }
    }
}

// === impl MakeFutures ===

impl<K: Clone + Eq + Hash, F: TryFuture> MakeFutures<K, F> {
    fn new() -> Self {
        Self {
            futures: FuturesUnordered::new(),
            cancelations: IndexMap::new(),
        }
    }

    fn push(&mut self, key: K, inner: F) {
        let (cancel, canceled) = oneshot::channel();
        if let Some(prior) = self.cancelations.insert(key.clone(), cancel) {
            let _ = prior.send(());
        }
        self.futures.push(MakeFuture {
            key: Some(key),
            inner,
            canceled,
        });
    }

    fn remove(&mut self, key: &K) {
        if let Some(cancel) = self.cancelations.remove(key) {
            let _ = cancel.send(());
        }
    }
}

impl<K: Eq + Hash, F: TryFuture> Stream for MakeFutures<K, F> {
    type Item = Result<(K, F::Ok), F::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let this = self.as_mut().project();
            return match ready!(this.futures.poll_next(cx)) {
                Some(Err(MakeError::Canceled)) => continue,
                Some(Err(MakeError::Inner(err))) => Poll::Ready(Some(Err(err))),
                Some(Ok((key, svc))) => {
                    let _rm = this.cancelations.remove(&key);
                    debug_assert!(_rm.is_some(), "cancelation missing");
                    Poll::Ready(Some(Ok((key, svc))))
                }
                None => Poll::Ready(None),
            };
        }
    }
}

// === impl MakeFuture ===

impl<K, F: TryFuture> Future for MakeFuture<K, F> {
    type Output = Result<(K, F::Ok), MakeError<F::Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        if let Poll::Ready(Ok(())) = this.canceled.poll(cx) {
            return Poll::Ready(Err(MakeError::Canceled));
        }
        let svc = ready!(this.inner.try_poll(cx))?;
        let key = this.key.take().expect("polled after complete");
        Poll::Ready(Ok((key, svc)))
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
    use std::net::SocketAddr;
    use tokio::sync::mpsc;
    use tokio_test::{assert_pending, assert_ready, assert_ready_ok, task};
    use tower::util::service_fn;
    use tower::Service;
    use tower_test::mock;

    #[derive(Debug)]
    struct Svc<F>(Vec<F>);
    impl<F, T, E> Service<()> for Svc<F>
    where
        F: Future<Output = Result<T, E>>,
    {
        type Response = T;
        type Error = E;
        type Future = F;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: ()) -> Self::Future {
            self.0.pop().expect("exhausted")
        }
    }

    #[pin_project]
    struct Dx(#[pin] mpsc::Receiver<Change<SocketAddr, ()>>);

    impl Stream for Dx {
        type Item = Result<Change<SocketAddr, ()>, Error>;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let change = ready!(self.project().0.poll_next(cx)).expect("stream must not end");
            Poll::Ready(Some(Ok(change)))
        }
    }

    #[test]
    fn inserts_delivered_out_of_order() {
        let (mut reso_tx, reso_rx) = mpsc::channel(2);
        let (make0_tx, make0_rx) = oneshot::channel::<Svc<oneshot::Receiver<usize>>>();
        let (make1_tx, make1_rx) = oneshot::channel::<Svc<oneshot::Receiver<usize>>>();

        let mut discover = task::spawn(Discover::new(Dx(reso_rx), Svc(vec![make1_rx, make0_rx])));
        assert_pending!(discover.poll_next(), "ready without updates");

        let addr0 = SocketAddr::from(([127, 0, 0, 1], 80));
        reso_tx.try_send(Change::Insert(addr0, ())).ok().unwrap();
        assert_pending!(discover.poll_next(), "ready without service being made");
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
            .try_send(Change::Insert(addr1, ()))
            .ok()
            .expect("update must be sent");
        assert_pending!(discover.poll_next(), "ready without service being made");
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
        match assert_ready!(discover.poll_next())
            .expect("discover stream mustn't end")
            .expect("discover can't fail")
        {
            Change::Remove(..) => panic!("unexpected remove"),
            Change::Insert(a, svc) => {
                assert_eq!(a, addr1);
                let mut svc = mock::Spawn::new(svc);
                assert_ready_ok!(svc.poll_ready());
                let mut fut = task::spawn(svc.call(()));
                assert_pending!(fut.poll());
                rsp1_tx.send(1).unwrap();
                assert_eq!(assert_ready_ok!(fut.poll()), 1);
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
        match assert_ready!(discover.poll_next())
            .expect("discover stream mustn't end")
            .expect("discover can't fail")
        {
            Change::Remove(..) => panic!("unexpected remove"),
            Change::Insert(a, svc) => {
                assert_eq!(a, addr0);
                let mut svc = mock::Spawn::new(svc);
                assert_ready_ok!(svc.poll_ready());
                let mut fut = task::spawn(svc.call(()));
                assert_pending!(fut.poll());
                rsp0_tx.send(0).unwrap();
                assert_eq!(assert_ready_ok!(fut.poll()), 0);
            }
        }
        assert!(discover.make_futures.futures.is_empty(), "futures remain");
        assert!(
            discover.make_futures.cancelations.is_empty(),
            "cancelation remains"
        );
    }

    #[test]
    fn overwriting_insert_cancels_original() {
        let (mut reso_tx, reso_rx) = mpsc::channel(2);
        let (make0_tx, make0_rx) = oneshot::channel::<Svc<oneshot::Receiver<usize>>>();
        let (make1_tx, make1_rx) = oneshot::channel::<Svc<oneshot::Receiver<usize>>>();

        let mut discover = task::spawn(Discover::new(Dx(reso_rx), Svc(vec![make1_rx, make0_rx])));
        assert_pending!(discover.poll_next(), "ready without updates");

        let addr = SocketAddr::from(([127, 0, 0, 1], 80));
        reso_tx.try_send(Change::Insert(addr, ())).ok().unwrap();
        assert_pending!(discover.poll_next(), "ready without service being made");
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
            .try_send(Change::Insert(addr, ()))
            .ok()
            .expect("update must be sent");
        assert_pending!(discover.poll_next(), "ready without service being made");
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
        match assert_ready!(discover.poll_next())
            .expect("discover stream mustn't end")
            .expect("discover can't fail")
        {
            Change::Remove(..) => panic!("unexpected remove"),
            Change::Insert(a, svc) => {
                assert_eq!(a, addr);
                let mut svc = mock::Spawn::new(svc);
                assert_ready_ok!(svc.poll_ready());
                let mut fut = task::spawn(svc.call(()));
                assert_pending!(fut.poll());
                rsp1_tx.send(1).unwrap();
                assert_eq!(assert_ready_ok!(fut.poll()), 1);
            }
        }
        assert!(
            discover.make_futures.cancelations.is_empty(),
            "cancelation remains"
        );
    }

    #[test]
    fn cancelation_of_pending_service() {
        let (mut tx, reso_rx) = mpsc::channel(1);

        let mut discover = task::spawn(Discover::new(
            Dx(reso_rx),
            service_fn(|()| future::pending::<Result<Svc<()>, Error>>()),
        ));
        assert_pending!(discover.poll_next(), "ready without updates");

        let addr = SocketAddr::from(([127, 0, 0, 1], 80));
        tx.try_send(Change::Insert(addr, ())).ok().unwrap();
        assert_pending!(discover.poll_next(), "ready without service being made");
        assert_eq!(
            discover.make_futures.cancelations.len(),
            1,
            "no pending cancelation"
        );

        tx.try_send(Change::Remove(addr)).ok().unwrap();
        match assert_ready!(discover.poll_next())
            .expect("discover stream mustn't end")
            .expect("discover can't fail")
        {
            Change::Insert(..) => panic!("unexpected insert"),
            Change::Remove(a) => assert_eq!(a, addr),
        }
        assert!(
            discover.make_futures.cancelations.is_empty(),
            "cancelation remains"
        );
    }
}

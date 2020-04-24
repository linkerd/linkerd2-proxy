use crate::error::ServiceError;
use crate::LockService;
use futures::{StreamExt, future};
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::{mpsc, oneshot};
use tokio_test::{assert_pending, assert_ready, assert_ready_ok};
use tower::Service as _Service;
use tower_test::mock::Spawn;
use tracing::{info_span, trace};
use tracing_futures::Instrument;

#[tokio::test]
async fn exclusive_access() {
    let ready = Arc::new(AtomicBool::new(false));
    let mut svc0 = Spawn::new(LockService::new(Decr::new(2, ready.clone())));

    // svc0 grabs the lock, but the inner service isn't ready.
    assert_pending!(svc0.poll_ready());

    // Cloning a locked service does not preserve the lock.
    let mut svc1 = svc0.clone();

    // svc1 can't grab the lock.
    assert_pending!(svc1.poll_ready());

    // svc0 holds the lock and becomes ready with the inner service.
    ready.store(true, Ordering::SeqCst);
    assert_ready_ok!(svc0.poll_ready());

    // svc1 still can't grab the lock.
    assert_pending!(svc1.poll_ready());

    // svc0 remains ready.
    let fut0 = svc0.call(1);

    // svc1 grabs the lock and is immediately ready.
    assert_ready_ok!(svc1.poll_ready());
    // svc0 cannot grab the lock.
    assert_pending!(svc0.poll_ready());

    let fut1 = svc1.call(1);

    tokio::try_join!(fut0, fut1).expect("must not fail!");
}

#[tokio::test]
async fn propagates_errors() {
    let mut svc0 = Spawn::new(LockService::new(Decr::from(1)));

    // svc0 grabs the lock and we decr the service so it will fail.
    assert_ready_ok!(svc0.poll_ready());

    // svc0 remains ready.
    let _ = svc0.call(1).await.expect("must not fail!");

    // svc1 grabs the lock and fails immediately.
    let mut svc1 = svc0.clone();
    assert_ready!(svc1.poll_ready())
        .expect_err("must fail")
        .downcast_ref::<ServiceError>()
        .expect("must fail with service error")
        .inner()
        .is::<Underflow>();

    // svc0 suffers the same fate.
    assert_ready!(svc0.poll_ready())
        .expect_err("mut fail")
        .downcast_ref::<ServiceError>()
        .expect("must fail with service error")
        .inner()
        .is::<Underflow>();
}

#[tokio::test]
async fn dropping_releases_access() {
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).try_init();
    use tower::util::ServiceExt;
    let mut svc0 = LockService::new(Decr::new(3, Arc::new(true.into())));

    // svc0 grabs the lock, but the inner service isn't ready.
    future::poll_fn(|cx| {
        assert_ready_ok!(svc0.poll_ready(cx));
        Poll::Ready(())
    })
    .await;

    let svc1 = svc0.clone();

    let (tx1, rx1) = oneshot::channel();
    tokio::spawn(
        async move {
            let svc1 = svc1;
            trace!("started");
            let _f = svc1.oneshot(1).instrument(info_span!("1shot")).await;
            trace!("sending");
            tx1.send(()).unwrap();
            trace!("done");
        }
        .instrument(info_span!("Svc1")),
    );

    let (tx2, rx2) = oneshot::channel();
    let svc2 = svc0.clone();
    tokio::spawn(
        async move {
            trace!("started");
            let _ = svc2.oneshot(1).await;
            trace!("sending");
            tx2.send(()).unwrap();
            trace!("done");
        }
        .instrument(info_span!("Svc2")),
    );

    // svc3 will be the notified waiter when svc0 completes; but it drops
    // svc3 before polling the waiter. This test ensures that svc2 is
    // notified by svc3's drop.
    let svc3 = svc0.clone();
    let (tx3, rx3) = oneshot::channel();
    tokio::spawn(
        async move {
            let mut svc3 = Some(svc3);
            let mut rx3 = rx3;
            let _ = future::poll_fn(|cx| {
                let rx3 = &mut rx3;
                tokio::pin!(rx3);
                trace!("Polling");
                if let Poll::Ready(ready) = rx3.poll(cx) {
                    trace!(?ready, "Dropping");
                    drop(svc3.take());
                    return Poll::Ready(Ok(()));
                }
                svc3.as_mut()
                    .expect("polled after ready?")
                    .poll_ready(cx)
                    .map_err(|_| ())
            })
            .await;
        }
        .instrument(info_span!("Svc3")),
    );

    tokio::spawn(
        async move {
            trace!("started");
            let _ = svc0.ready_and().await;
            trace!("ready");
            tx3.send(()).map_err(|_| ())?;
            trace!("sent");
            Ok::<(), ()>(())
        }
        .instrument(info_span!("Svc0")),
    );
    // svc3 notified; but it is dropped before it can be polled

    rx2.await.unwrap();
    rx1.await.unwrap();
}

#[tokio::test]
async fn fuzz() {
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).try_init();
    const ITERS: usize = 100_000;
    for (concurrency, iterations) in &[(1usize, ITERS), (3, ITERS), (100, ITERS)] {
        async {
            tracing::info!("starting");
            let svc = LockService::new(Decr::new(*iterations, Arc::new(true.into())));
            let (tx, rx) = mpsc::channel(1);
            for i in 0..*concurrency {
                let lock = svc.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut lock = lock;
                    let _tx = tx;
                    future::poll_fn::<Result<(),()>, _>(|cx| {
                        loop {
                            futures::ready!(lock.poll_ready(cx)).map_err(|_|())?;

                            // Randomly be busy while holding the lock.
                            if rand::random::<bool>() {
                                cx.waker().wake_by_ref();
                                return Poll::Pending;
                            }

                            tokio::spawn(lock.call(1));
                        }
                    }).await
                }.instrument(tracing::trace_span!("task", number = i)));
            }

            rx.fold((), |(), ()| async { () }).await;
            tracing::info!("done");
        }.instrument(tracing::info_span!("fuzz", concurrency, iterations))
        .await
    }
}

#[derive(Debug, Default)]
struct Decr {
    value: usize,
    ready: Arc<AtomicBool>,
}

#[derive(Copy, Clone, Debug)]
struct Underflow;

impl From<usize> for Decr {
    fn from(value: usize) -> Self {
        Self::new(value, Arc::new(AtomicBool::new(true)))
    }
}

impl Decr {
    fn new(value: usize, ready: Arc<AtomicBool>) -> Self {
        Decr { value, ready }
    }
}

impl tower::Service<usize> for Decr {
    type Response = usize;
    type Error = Underflow;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + Sync + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let span = tracing::trace_span!("Decr::poll_ready", self.value);
        let _g = span.enter();
        if self.value == 0 {
            tracing::trace!(ready = true, "underflow");
            return Poll::Ready(Err(Underflow));
        }

        if !self.ready.load(Ordering::SeqCst) {
            tracing::trace!(ready = false);
            return Poll::Pending;
        }

        tracing::trace!(ready = true);
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, decr: usize) -> Self::Future {
        if self.value < decr {
            self.value = 0;

            return Box::pin(async { Err(Underflow) });
        }

        self.value -= decr;
        let value = self.value;
        Box::pin(async move { Ok(value) })
    }
}

impl std::fmt::Display for Underflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "underflow")
    }
}

impl std::error::Error for Underflow {}

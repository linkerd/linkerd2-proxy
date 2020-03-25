use crate::error::ServiceError;
use crate::LockService;
use futures::{future, try_ready, Async, Future, Poll, Stream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tower::Service as _Service;
use tracing::{info_span, trace};
use tracing_futures::Instrument;

#[test]
fn exclusive_access() {
    run(future::lazy(|| {
        let ready = Arc::new(AtomicBool::new(false));
        let mut svc0 = LockService::new(Decr::new(2, ready.clone()));

        // svc0 grabs the lock, but the inner service isn't ready.
        assert!(svc0.poll_ready().expect("must not fail").is_not_ready());

        // Cloning a locked service does not preserve the lock.
        let mut svc1 = svc0.clone();

        // svc1 can't grab the lock.
        assert!(svc1.poll_ready().expect("must not fail").is_not_ready());

        // svc0 holds the lock and becomes ready with the inner service.
        ready.store(true, Ordering::SeqCst);
        assert!(svc0.poll_ready().expect("must not fail").is_ready());

        // svc1 still can't grab the lock.
        assert!(svc1.poll_ready().expect("must not fail").is_not_ready());

        // svc0 remains ready.
        let fut0 = svc0.call(1);

        // svc1 grabs the lock and is immediately ready.
        assert!(svc1.poll_ready().expect("must not fail").is_ready());
        // svc0 cannot grab the lock.
        assert!(svc0.poll_ready().expect("must not fail").is_not_ready());

        let fut1 = svc1.call(1);

        fut0.join(fut1)
            .map(|_| ())
            .map_err(|_| panic!("must not fail"))
    }));
}

#[test]
fn propagates_errors() {
    run(future::lazy(|| {
        let mut svc0 = LockService::new(Decr::from(1));

        // svc0 grabs the lock and we decr the service so it will fail.
        assert!(svc0.poll_ready().expect("must not fail").is_ready());
        // svc0 remains ready.
        svc0.call(1)
            .map_err(|_| panic!("must not fail"))
            .map(move |_| {
                // svc1 grabs the lock and fails immediately.
                let mut svc1 = svc0.clone();
                assert!(svc1
                    .poll_ready()
                    .expect_err("mut fail")
                    .downcast_ref::<ServiceError>()
                    .expect("must fail with service error")
                    .inner()
                    .is::<Underflow>());

                // svc0 suffers the same fate.
                assert!(svc0
                    .poll_ready()
                    .expect_err("mut fail")
                    .downcast_ref::<ServiceError>()
                    .expect("must fail with service error")
                    .inner()
                    .is::<Underflow>());
            })
    }));
}

#[test]
fn dropping_releases_access() {
    use tower::util::ServiceExt;
    run(future::lazy(|| {
        let mut svc0 = LockService::new(Decr::new(3, Arc::new(true.into())));

        // svc0 grabs the lock, but the inner service isn't ready.
        assert!(svc0.poll_ready().expect("must not fail").is_ready());

        let svc1 = svc0.clone();
        let (tx1, rx1) = oneshot::channel();
        tokio::spawn(
            svc1.oneshot(1)
                .then(move |_| {
                    trace!("complete");
                    tx1.send(()).map_err(|_| ())
                })
                .instrument(info_span!("Svc1")),
        );

        let svc2 = svc0.clone();
        let (tx2, rx2) = oneshot::channel();
        tokio::spawn(
            svc2.oneshot(1)
                .then(move |_| {
                    trace!("complete");
                    tx2.send(()).map_err(|_| ())
                })
                .instrument(info_span!("Svc2")),
        );

        // svc3 will be the notified waiter when svc0 completes; but it drops
        // svc3 before polling the waiter. This test ensures that svc2 is
        // notified by svc3's drop.
        let svc3 = svc0.clone();
        let (tx3, rx3) = oneshot::channel();
        tokio::spawn(PollAndDrop(svc3, rx3).instrument(info_span!("Svc3")));

        tokio::spawn(
            svc0.ready()
                .then(move |_| tx3.send(()).map_err(|_| ()))
                .instrument(info_span!("Svc0")),
        );
        // svc3 notified; but it is dropped before it can be polled

        rx2.then(move |_| rx1).map_err(|_| ())
    }));

    struct PollAndDrop(LockService<Decr>, oneshot::Receiver<()>);
    impl Future for PollAndDrop {
        type Item = ();
        type Error = ();
        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            trace!("Polling");
            if self.1.poll().map_err(|_| ())?.is_ready() {
                trace!("Dropping");
                return Ok(Async::Ready(()));
            }
            self.0.poll_ready().map_err(|_| ())
        }
    }
}

#[test]
fn fuzz() {
    const ITERS: usize = 100_000;
    for (concurrency, iterations) in &[(1, ITERS), (3, ITERS), (100, ITERS)] {
        tokio::run(future::lazy(move || {
            let svc = LockService::new(Decr::new(*iterations, Arc::new(true.into())));
            let (tx, rx) = mpsc::channel(1);
            for _ in 0..*concurrency {
                tokio::spawn(Loop {
                    lock: svc.clone(),
                    _tx: tx.clone(),
                });
            }
            rx.fold((), |(), ()| Ok(())).map_err(|_| ())
        }));
    }

    struct Loop {
        lock: LockService<Decr>,
        _tx: mpsc::Sender<()>,
    }
    impl Future for Loop {
        type Item = ();
        type Error = ();

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            loop {
                try_ready!(self.lock.poll_ready().map_err(|_| ()));

                // Randomly be busy while holding the lock.
                if rand::random::<bool>() {
                    futures::task::current().notify();
                    return Ok(Async::NotReady);
                }

                tokio::spawn(self.lock.call(1).then(|_| Ok(())));
            }
        }
    }
}

fn run<F>(future: F)
where
    F: Future<Item = (), Error = ()> + 'static,
{
    let subscriber = tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stdout)
        .finish();
    tracing::subscriber::with_default(subscriber, move || {
        tokio::runtime::current_thread::run(future)
    });
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
    type Future = futures::future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> futures::Poll<(), Self::Error> {
        if self.value == 0 {
            return Err(Underflow);
        }

        if !self.ready.load(Ordering::SeqCst) {
            return Ok(Async::NotReady);
        }

        Ok(().into())
    }

    fn call(&mut self, decr: usize) -> Self::Future {
        if self.value < decr {
            self.value = 0;
            return futures::future::err(Underflow);
        }

        self.value -= decr;
        futures::future::ok(self.value)
    }
}

impl std::fmt::Display for Underflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "underflow")
    }
}

impl std::error::Error for Underflow {}

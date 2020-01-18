//! A middleware for sharing an inner service via mutual exclusion.

#![deny(warnings, rust_2018_idioms)]

use futures::{future, Async, Future, Poll};
use linkerd2_error::Error;
use tokio::sync::lock;
use tracing::trace;

#[derive(Clone, Debug)]
pub struct Layer<E = Poisoned> {
    _marker: std::marker::PhantomData<E>,
}

/// Guards access to an inner service with a `tokio::sync::lock::Lock`.
///
/// As the service is polled to readiness, the lock is acquired and the inner
/// service is polled. If the sevice is cloned, the service's lock state is not
/// retained by the clone.
///
/// The inner service's errors are coerced to the cloneable `C`-typed error so
/// that the error may be returned to all clones of the lock. By default, errors
/// are propagated through the `Poisoned` type, but they may be propagated
/// through custom types as well.
pub struct Lock<S, E = Poisoned> {
    lock: lock::Lock<State<S, E>>,
    locked: Option<lock::LockGuard<State<S, E>>>,
}

/// The lock holds either an inner srvice or, if it failed, an error salvaged
/// from the failure.
enum State<S, E> {
    Service(S),
    Error(E),
}

/// A default error type that propagates the inner service's error message to
/// consumers.
#[derive(Clone, Debug)]
pub struct Poisoned(String);

// === impl Layer ===

impl Default for Layer {
    fn default() -> Self {
        Self::new()
    }
}

impl Layer {
    /// Sets the error type to be returned to consumers when poll_ready fails.
    pub fn new<E>() -> Layer<E>
    where
        E: Clone + From<Error> + Into<Error>,
    {
        Layer {
            _marker: std::marker::PhantomData,
        }
    }
}

impl<S, E: From<Error> + Clone> tower::layer::Layer<S> for Layer<E> {
    type Service = Lock<S, E>;

    fn layer(&self, inner: S) -> Self::Service {
        Self::Service {
            lock: lock::Lock::new(State::Service(inner)),
            locked: None,
        }
    }
}

// === impl Lock ===

impl<S, E> Clone for Lock<S, E> {
    fn clone(&self) -> Self {
        Self {
            locked: None,
            lock: self.lock.clone(),
        }
    }
}

impl<T, S, E> tower::Service<T> for Lock<S, E>
where
    S: tower::Service<T>,
    S::Error: Into<Error>,
    E: Clone + From<Error> + Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::MapErr<S::Future, fn(S::Error) -> Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        loop {
            if let Some(state) = self.locked.as_mut() {
                // Drive the service to readiness if it is already locked.
                if let State::Service(ref mut inner) = **state {
                    return match inner.poll_ready() {
                        Ok(ok) => {
                            trace!(ready = ok.is_ready(), "service");
                            Ok(ok)
                        }
                        Err(inner) => {
                            // Coerce the error into `E` and clone it into the
                            // locked state so that it can be returned from all
                            // clones of the lock.
                            let err = E::from(inner.into());
                            **state = State::Error(err.clone());
                            self.locked = None;
                            Err(err.into())
                        }
                    };
                }

                // If an error occured above,the locked state is dropped and
                // cannot be acquired again.
                unreachable!("must not lock on error");
            }

            // Acquire the inner service exclusively so that the service can be
            // driven to readiness.
            match self.lock.poll_lock() {
                Async::NotReady => {
                    trace!(locked = false);
                    return Ok(Async::NotReady);
                }
                Async::Ready(locked) => {
                    if let State::Error(ref e) = *locked {
                        return Err(e.clone().into());
                    }

                    trace!(locked = true);
                    self.locked = Some(locked);
                }
            }
        }
    }

    fn call(&mut self, t: T) -> Self::Future {
        if let Some(mut state) = self.locked.take() {
            if let State::Service(ref mut inner) = *state {
                return inner.call(t).map_err(Into::into);
            }
        }

        unreachable!("called before ready");
    }
}

// === impl Poisoned ===

impl From<Error> for Poisoned {
    fn from(e: Error) -> Self {
        Poisoned(e.to_string())
    }
}

impl std::fmt::Display for Poisoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for Poisoned {}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::runtime::current_thread;
    use tower::layer::Layer as _Layer;
    use tower::Service as _Service;

    #[test]
    fn exclusive_access() {
        current_thread::run(future::lazy(|| {
            let ready = Arc::new(AtomicBool::new(false));
            let mut svc0 = Layer::default().layer(Decr::new(2, ready.clone()));

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
            svc0.call(1)
                .and_then(move |_| {
                    // svc1 grabs the lock and is immediately ready.
                    assert!(svc1.poll_ready().expect("must not fail").is_ready());
                    // svc0 cannot grab the lock.
                    assert!(svc0.poll_ready().expect("must not fail").is_not_ready());

                    svc1.call(1)
                })
                .map(|_| ())
                .map_err(|_| panic!("must not fail"))
        }));
    }

    #[test]
    fn propagates_poisoned_errors() {
        current_thread::run(future::lazy(|| {
            let mut svc0 = Layer::default().layer(Decr::from(1));

            // svc0 grabs the lock and we decr the service so it will fail.
            assert!(svc0.poll_ready().expect("must not fail").is_ready());
            // svc0 remains ready.
            svc0.call(1)
                .map_err(|_| panic!("must not fail"))
                .map(move |_| {
                    // svc1 grabs the lock and fails immediately.
                    let mut svc1 = svc0.clone();
                    assert_eq!(
                        svc1.poll_ready()
                            .expect_err("mut fail")
                            .downcast_ref::<Poisoned>()
                            .expect("must fail with Poisoned")
                            .to_string(),
                        "underflow"
                    );

                    // svc0 suffers the same fate.
                    assert_eq!(
                        svc0.poll_ready()
                            .expect_err("mut fail")
                            .downcast_ref::<Poisoned>()
                            .expect("must fail with Poisoned")
                            .to_string(),
                        "underflow"
                    );
                })
        }));
    }

    #[test]
    fn propagates_custom_errors() {
        current_thread::run(future::lazy(|| {
            #[derive(Clone, Debug)]
            enum Custom {
                Underflow,
                Wtf,
            }
            impl From<Error> for Custom {
                fn from(e: Error) -> Self {
                    if e.is::<Underflow>() {
                        Custom::Underflow
                    } else {
                        Custom::Wtf
                    }
                }
            }
            impl std::fmt::Display for Custom {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    match self {
                        Custom::Underflow => write!(f, "custom underflow"),
                        Custom::Wtf => write!(f, "wtf"),
                    }
                }
            }
            impl std::error::Error for Custom {}

            let mut svc0 = Layer::new::<Custom>().layer(Decr::from(1));

            // svc0 grabs the lock and we decr the service so it will fail.
            assert!(svc0.poll_ready().expect("must not fail").is_ready());
            // svc0 remains ready.
            svc0.call(1)
                .map_err(|_| panic!("must not fail"))
                .map(move |_| {
                    let mut svc1 = svc0.clone();
                    // svc1 grabs the lock and fails immediately.
                    assert_eq!(
                        svc1.poll_ready()
                            .expect_err("mut fail")
                            .downcast_ref::<Custom>()
                            .expect("must fail with Custom")
                            .to_string(),
                        "custom underflow"
                    );

                    // svc0 suffers the same fate.
                    assert_eq!(
                        svc0.poll_ready()
                            .expect_err("mut fail")
                            .downcast_ref::<Custom>()
                            .expect("must fail with Custom")
                            .to_string(),
                        "custom underflow"
                    );
                })
        }));
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
}

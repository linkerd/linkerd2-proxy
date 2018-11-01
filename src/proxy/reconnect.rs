extern crate tower_reconnect;

use futures::{task, Async, Future, Poll};
use std::fmt;
use std::time::Duration;
pub use self::tower_reconnect::{Error, Reconnect};
use tokio_timer::{clock, Delay};

use svc;

#[derive(Clone, Debug)]
pub struct Layer {
    backoff: Backoff,
}

#[derive(Clone, Debug)]
pub struct Stack<M> {
    backoff: Backoff,
    inner: M,
}

/// Wraps `tower_reconnect`, handling errors.
///
/// Ensures that the underlying service is ready and, if the underlying service
/// fails to become ready, rebuilds the inner stack.
pub struct Service<T, N>
where
    T: fmt::Debug,
    N: svc::NewService,
{
    inner: Reconnect<N>,

    /// The target, used for debug logging.
    target: T,

    backoff: Backoff,
    active_backoff: Option<Delay>,

    /// Prevents logging repeated connect errors.
    ///
    /// Set back to false after a connect succeeds, to log about future errors.
    mute_connect_error_log: bool,
}

#[derive(Clone, Debug)]
enum Backoff {
    None,
    Fixed(Duration),
}

pub struct ResponseFuture<N: svc::NewService> {
    inner: <Reconnect<N> as svc::Service>::Future,
}

// === impl Layer ===

pub fn layer() -> Layer {
    Layer {
        backoff: Backoff::None,
    }
}

impl Layer {
    pub fn with_fixed_backoff(self, wait: Duration) -> Self {
        Self {
            backoff: Backoff::Fixed(wait),
            .. self
        }
    }
}

impl<T, M> svc::Layer<T, T, M> for Layer
where
    T: Clone + fmt::Debug,
    M: svc::Stack<T>,
    M::Value: svc::NewService,
{
    type Value = <Stack<M> as svc::Stack<T>>::Value;
    type Error = <Stack<M> as svc::Stack<T>>::Error;
    type Stack = Stack<M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            inner,
            backoff: self.backoff.clone(),
        }
    }
}

// === impl Stack ===

impl<T, M> svc::Stack<T> for Stack<M>
where
    T: Clone + fmt::Debug,
    M: svc::Stack<T>,
    M::Value: svc::NewService,
{
    type Value = Service<T, M::Value>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let new_service = self.inner.make(target)?;
        Ok(Service {
            inner: Reconnect::new(new_service),
            target: target.clone(),
            backoff: self.backoff.clone(),
            active_backoff: None,
            mute_connect_error_log: false,
        })
    }
}

// === impl Service ===

#[cfg(test)]
impl<N> Service<&'static str, N>
where
    N: svc::NewService,
    N::InitError: fmt::Display,
{
    fn for_test(new_service: N) -> Self {
        Self {
            inner: Reconnect::new(new_service),
            target: "test",
            backoff: Backoff::None,
            active_backoff: None,
            mute_connect_error_log: false,
        }
    }

    fn with_fixed_backoff(self, wait: Duration) -> Self {
        Self {
            backoff: Backoff::Fixed(wait),
            .. self
        }
    }
}

impl<T, N> svc::Service for Service<T, N>
where
    T: fmt::Debug,
    N: svc::NewService,
    N::InitError: fmt::Display,
{
    type Request = N::Request;
    type Response = N::Response;
    type Error = N::Error;
    type Future = ResponseFuture<N>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match self.backoff {
            Backoff::None => {}
            Backoff::Fixed(_) => {
                if let Some(delay) = self.active_backoff.as_mut() {
                    match delay.poll() {
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Ok(Async::Ready(())) => {},
                        Err(e) => {
                            error!("timer failed; continuing without backoff: {}", e);
                        }
                    }
                }
            }
        };
        self.active_backoff = None;

        match self.inner.poll_ready() {
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(ready) => {
                self.mute_connect_error_log = false;
                Ok(ready)
            }

            Err(Error::Inner(err)) => {
                self.mute_connect_error_log = false;
                Err(err)
            }

            Err(Error::Connect(err)) => {
                // A connection could not be established to the target.

                // This is only logged as a warning at most once. Subsequent
                // errors are logged at debug.
                if !self.mute_connect_error_log {
                    self.mute_connect_error_log = true;
                    warn!("connect error to {:?}: {}", self.target, err);
                } else {
                    debug!("connect error to {:?}: {}", self.target, err);
                }

                // Set a backoff if appropriate.
                //
                // This future need not be polled immediately because the
                // task is notified below.
                self.active_backoff = match self.backoff {
                    Backoff::None => None,
                    Backoff::Fixed(ref wait) => Some(Delay::new(clock::now() + *wait)),
                };

                // The inner service is now idle and will renew its internal
                // state on the next poll. Instead of doing this immediately,
                // the task is scheduled to be polled again only if the caller
                // decides not to drop it.
                //
                // This prevents busy-looping when the connect error is
                // instantaneous.
                task::current().notify();
                Ok(Async::NotReady)
            }

            Err(Error::NotReady) => {
                unreachable!("poll_ready can't fail with NotReady");
            }
        }
    }

    fn call(&mut self, request: Self::Request) -> Self::Future {
        ResponseFuture {
            inner: self.inner.call(request),
        }
    }
}

impl<T: fmt::Debug, N: svc::NewService> fmt::Debug for Service<T, N> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Reconnect")
            .field("target", &self.target)
            .finish()
    }
}

impl<N: svc::NewService> Future for ResponseFuture<N> {
    type Item = N::Response;
    type Error = N::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.inner.poll().map_err(|e| match e {
            Error::Inner(err) => err,
            _ => unreachable!("response future must fail with inner error"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{future, Future};
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    use svc::Service as _Service;
    use std::{error, fmt, time};
    use tokio::runtime::current_thread::Runtime;

    struct NewService {
        fails: AtomicUsize,
    }

    struct Service {}

    struct InitFuture {
        should_fail: bool,
    }

    #[derive(Debug)]
    struct InitErr {}

    impl svc::NewService for NewService {
        type Request = ();
        type Response = ();
        type Error = ();
        type Service = Service;
        type InitError = InitErr;
        type Future = InitFuture;

        fn new_service(&self) -> Self::Future {
            InitFuture {
                should_fail: self.fails.fetch_sub(1, Relaxed) > 0,
            }
        }
    }

    impl svc::Service for Service {
        type Request = ();
        type Response = ();
        type Error = ();
        type Future = future::FutureResult<(), ()>;

        fn poll_ready(&mut self) -> Poll<(), ()> {
            Ok(().into())
        }

        fn call(&mut self, _req: ()) -> Self::Future {
            future::ok(())
        }
    }

    impl Future for InitFuture {
        type Item = Service;
        type Error = InitErr;

        fn poll(&mut self) -> Poll<Service, InitErr> {
            if self.should_fail {
                return Err(InitErr {})
            }

            Ok(Service{}.into())
        }
    }

    impl fmt::Display for InitErr {
        fn fmt(&self, _: &mut fmt::Formatter) -> fmt::Result {
            Ok(())
        }
    }
    impl error::Error for InitErr {}

    #[test]
    fn reconnects_with_backoff() {
        let mock = NewService { fails: 2.into() };
        let mut backoff = super::Service::for_test(mock)
            .with_fixed_backoff(Duration::from_millis(100));
        let mut rt = Runtime::new().unwrap();

        // Checks that, after the inner NewService fails to connect twice, it
        // succeeds on a third attempt.
        let t0 = time::Instant::now();
        let f = future::poll_fn(|| backoff.poll_ready());
        rt.block_on(f).unwrap();

        assert!(t0.elapsed() >= Duration::from_millis(200))
    }
}

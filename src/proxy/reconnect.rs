extern crate tower_reconnect;

pub use self::tower_reconnect::Reconnect;
use futures::{task, Async, Future, Poll};
use std::fmt;
use std::marker::PhantomData;
use std::time::Duration;
use tokio_timer::{clock, Delay};

use svc;

// compiler doesn't seem to notice this used in where bounds below...
#[allow(unused)]
type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug)]
pub struct Layer<Req> {
    backoff: Backoff,
    _req: PhantomData<fn(Req)>,
}

#[derive(Debug)]
pub struct Stack<Req, M> {
    backoff: Backoff,
    inner: M,
    _req: PhantomData<fn(Req)>,
}

/// Wraps `tower_reconnect`, handling errors.
///
/// Ensures that the underlying service is ready and, if the underlying service
/// fails to become ready, rebuilds the inner stack.
pub struct Service<T, N>
where
    T: fmt::Debug,
    N: svc::Service<()>,
{
    inner: Reconnect<N, ()>,

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

// === impl Layer ===

pub fn layer<Req>() -> Layer<Req> {
    Layer {
        backoff: Backoff::None,
        _req: PhantomData,
    }
}

impl<Req> Layer<Req> {
    pub fn with_fixed_backoff(self, wait: Duration) -> Self {
        Self {
            backoff: Backoff::Fixed(wait),
            _req: PhantomData,
        }
    }
}

impl<Req> Clone for Layer<Req> {
    fn clone(&self) -> Self {
        Layer {
            backoff: self.backoff.clone(),
            _req: PhantomData,
        }
    }
}

impl<Req, T, M, N, S> svc::Layer<T, T, M> for Layer<Req>
where
    T: Clone + fmt::Debug,
    M: svc::Stack<T, Value = N>,
    N: svc::Service<(), Response = S>,
    S: svc::Service<Req>,
    Error: From<N::Error> + From<S::Error>,
{
    type Value = <Stack<Req, M> as svc::Stack<T>>::Value;
    type Error = <Stack<Req, M> as svc::Stack<T>>::Error;
    type Stack = Stack<Req, M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            inner,
            backoff: self.backoff.clone(),
            _req: PhantomData,
        }
    }
}

// === impl Stack ===

impl<Req, M: Clone> Clone for Stack<Req, M> {
    fn clone(&self) -> Self {
        Stack {
            inner: self.inner.clone(),
            backoff: self.backoff.clone(),
            _req: PhantomData,
        }
    }
}

impl<T, Req, M, N, S> svc::Stack<T> for Stack<Req, M>
where
    T: Clone + fmt::Debug,
    M: svc::Stack<T, Value = N>,
    N: svc::Service<(), Response = S>,
    S: svc::Service<Req>,
    Error: From<N::Error> + From<S::Error>,
{
    type Value = Service<T, M::Value>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let new_service = self.inner.make(target)?;
        Ok(Service {
            inner: Reconnect::new(new_service, ()),
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
    N: svc::Service<()>,
    N::Error: fmt::Display,
{
    fn for_test(new_service: N) -> Self {
        Self {
            inner: Reconnect::new(new_service, ()),
            target: "test",
            backoff: Backoff::None,
            active_backoff: None,
            mute_connect_error_log: false,
        }
    }

    fn with_fixed_backoff(self, wait: Duration) -> Self {
        Self {
            backoff: Backoff::Fixed(wait),
            ..self
        }
    }
}

impl<T, N, S, Req> svc::Service<Req> for Service<T, N>
where
    T: fmt::Debug,
    N: svc::Service<(), Response = S>,
    S: svc::Service<Req>,
    Error: From<N::Error> + From<S::Error>,
{
    type Response = S::Response;
    type Error = <Reconnect<N, ()> as svc::Service<Req>>::Error;
    type Future = <Reconnect<N, ()> as svc::Service<Req>>::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match self.backoff {
            Backoff::None => {}
            Backoff::Fixed(_) => {
                if let Some(delay) = self.active_backoff.as_mut() {
                    match delay.poll() {
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Ok(Async::Ready(())) => {}
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
            Err(err) => {
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
        }
    }

    fn call(&mut self, request: Req) -> Self::Future {
        self.inner.call(request)
    }
}

impl<T, N> fmt::Debug for Service<T, N>
where
    T: fmt::Debug,
    N: svc::Service<()>,
{
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Reconnect")
            .field("target", &self.target)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{future, Future};
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    use std::{error, fmt, time};
    use svc::Service as _Service;
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

    impl svc::Service<()> for NewService {
        type Response = Service;
        type Error = InitErr;
        type Future = InitFuture;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            Ok(().into())
        }

        fn call(&mut self, _: ()) -> Self::Future {
            InitFuture {
                should_fail: self.fails.fetch_sub(1, Relaxed) > 0,
            }
        }
    }

    impl svc::Service<()> for Service {
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
                return Err(InitErr {});
            }

            Ok(Service {}.into())
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
        let mut backoff =
            super::Service::for_test(mock).with_fixed_backoff(Duration::from_millis(100));
        let mut rt = Runtime::new().unwrap();

        // Checks that, after the inner NewService fails to connect twice, it
        // succeeds on a third attempt.
        let t0 = time::Instant::now();
        let f = future::poll_fn(|| backoff.poll_ready());
        rt.block_on(f).unwrap();

        assert!(t0.elapsed() >= Duration::from_millis(200))
    }
}

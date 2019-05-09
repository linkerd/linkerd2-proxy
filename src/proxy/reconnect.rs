extern crate tower_reconnect;

use rand;
use std::fmt;
use std::marker::PhantomData;
use std::ops::Mul;
use std::time::Duration;

use self::tower_reconnect::Reconnect;
use futures::{task, Async, Future, Poll};
use tokio_timer::{clock, Delay};

use proxy::Error;
use svc;

#[derive(Debug)]
pub struct Layer<Req> {
    backoff: Backoff,
    _req: PhantomData<fn(Req)>,
}

#[derive(Debug)]
pub struct MakeReconnect<Req, M> {
    backoff: Backoff,
    inner: M,
    _req: PhantomData<fn(Req)>,
}

pub struct Service<M, T>
where
    M: svc::Service<T>,
{
    inner: Reconnect<M, T>,

    /// The target, used for debug logging.
    target: T,

    backoff: Backoff,
    active_backoff: Option<Delay>,
    failed_attempts: u32,

    /// Prevents logging repeated connect errors.
    ///
    /// Set back to false after a connect succeeds, to log about future errors.
    mute_connect_error_log: bool,
}

#[derive(Clone, Debug)]
pub enum Backoff {
    None,
    Exponential {
        min: Duration,
        max: Duration,
        jitter: f64,
    },
}

impl Backoff {
    fn for_failures<R: rand::Rng>(&self, failures: u32, mut rng: R) -> Option<Duration> {
        match self {
            Backoff::None => None,
            Backoff::Exponential { max, min, jitter } => {
                let backoff = Duration::min(min.mul(2_u32.saturating_pow(failures)), *max);

                if *jitter != 0.0 {
                    Some(backoff)
                } else {
                    let jitter_factor = rng.gen::<f64>();
                    debug_assert!(
                        jitter_factor > 0.0,
                        "rng returns values between 0.0 and 1.0"
                    );
                    let rand_jitter = jitter_factor * jitter;
                    let secs = (backoff.as_secs() as f64) * rand_jitter;
                    let nanos = (backoff.subsec_nanos() as f64) * rand_jitter;
                    let millis = secs.mul_add(secs * 1000f64, nanos / 1000000f64);
                    Some(backoff + Duration::from_millis(millis as u64))
                }
            }
        }
    }
}

// === impl Layer ===

pub fn layer<Req>() -> Layer<Req> {
    Layer {
        backoff: Backoff::None,
        _req: PhantomData,
    }
}

impl<Req> Layer<Req> {
    pub fn with_backoff(self, backoff: Backoff) -> Self {
        Self {
            backoff,
            _req: PhantomData,
        }
    }
}

impl<Req, M> svc::Layer<M> for Layer<Req> {
    type Service = MakeReconnect<Req, M>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeReconnect {
            inner,
            backoff: self.backoff.clone(),
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

// === impl Service ===

// === impl MakeReconnect ===

impl<Req, M: Clone> Clone for MakeReconnect<Req, M> {
    fn clone(&self) -> Self {
        MakeReconnect {
            inner: self.inner.clone(),
            backoff: self.backoff.clone(),
            _req: PhantomData,
        }
    }
}

impl<Req, M, T, S> svc::Service<T> for MakeReconnect<Req, M>
where
    T: fmt::Debug + Clone,
    M: svc::Service<T, Response = S> + Clone,
    S: svc::Service<Req>,
    Error: From<M::Error> + From<S::Error>,
{
    type Response = Service<M, T>;
    type Error = never::Never;
    type Future = futures::future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    fn call(&mut self, target: T) -> Self::Future {
        futures::future::ok(Service {
            inner: Reconnect::new(self.inner.clone(), target.clone()),
            target,
            backoff: self.backoff.clone(),
            active_backoff: None,
            mute_connect_error_log: false,
            failed_attempts: 0,
        })
    }
}

// === impl Service ===

#[cfg(test)]
impl<M, S> Service<M, ()>
where
    M: svc::Service<(), Response = S>,
    M::Error: Send + Sync,
    S: svc::Service<()>,
    S::Error: Send + Sync,
    Error: From<M::Error> + From<S::Error>,
{
    fn for_test(new_service: M) -> Self {
        Self {
            inner: Reconnect::new(new_service, ()),
            target: (),
            backoff: Backoff::None,
            active_backoff: None,
            mute_connect_error_log: false,
            failed_attempts: 0,
        }
    }

    fn with_backoff(self, backoff: Backoff) -> Self {
        Self { backoff, ..self }
    }
}

impl<T, M, S, Req> svc::Service<Req> for Service<M, T>
where
    T: fmt::Debug + Clone,
    M: svc::Service<T, Response = S>,
    S: svc::Service<Req>,
    Error: From<M::Error> + From<S::Error>,
{
    type Response = S::Response;
    type Error = <Reconnect<M, T> as svc::Service<Req>>::Error;
    type Future = <Reconnect<M, T> as svc::Service<Req>>::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        if let Some(delay) = self.active_backoff.as_mut() {
            match delay.poll() {
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Ok(Async::Ready(())) => {}
                Err(e) => {
                    error!("timer failed; continuing without backoff: {}", e);
                }
            }
        }
        self.active_backoff = None;

        match self.inner.poll_ready() {
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(ready) => {
                self.failed_attempts = 0;
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
                self.active_backoff = self
                    .backoff
                    .for_failures(self.failed_attempts, rand::thread_rng())
                    .map(|backoff| {
                        debug!("reconnect backoff waiting for {:?}", backoff);
                        Delay::new(clock::now() + backoff)
                    });

                self.failed_attempts += 1;

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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{future, Future};
    use never::Never;
    use quickcheck::*;
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
        type Error = Never;
        type Future = future::FutureResult<(), Self::Error>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
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
    fn reconnects_with_exp_backoff() {
        let mock = NewService { fails: 3.into() };
        let mut backoff = super::Service::for_test(mock).with_backoff(Backoff::Exponential {
            min: Duration::from_millis(100),
            max: Duration::from_millis(300),
            jitter: 0.0,
        });
        let mut rt = Runtime::new().unwrap();

        // Checks that, after the inner NewService fails to connect three times, it
        // succeeds on a fourth attempt.
        let t0 = time::Instant::now();
        let f = future::poll_fn(|| backoff.poll_ready());
        rt.block_on(f).unwrap();
        assert!(t0.elapsed() >= Duration::from_millis(600));
    }

    quickcheck! {
        fn backoff_for_failures(
            exp: bool,
            min: u64,
            max: u64,
            failures: u32,
            jitter: f64
        ) -> bool {
            let backoff = if exp {Backoff::Exponential {min: Duration::from_millis(min),max: Duration::from_millis(max), jitter }} else { Backoff::None};
            backoff.for_failures(failures,rand::thread_rng());

            true
        }
    }
}

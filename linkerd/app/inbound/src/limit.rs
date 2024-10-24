use futures::{future, Future, TryFutureExt};
use linkerd_error::Error;
use linkerd_app_core::{
    svc,
    transport::{ClientAddr, Remote},
};
use tokio::time::{Instant, Sleep};
use thiserror::Error;
use std::{
    pin::Pin,
    task,
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct NewRateLimitPolicy<N> {
    inner: N,
}

#[derive(Debug)]
pub struct RateLimitPolicyService<N> {
    client: Remote<ClientAddr>,
    rate: Rate,
    state: State,
    sleep: Pin<Box<Sleep>>,
    inner: N,
}

#[derive(Debug, Error)]
#[error("too many requests")]
pub struct RateLimitError(());

/// A rate of requests per time period.
#[derive(Debug, Copy, Clone)]
pub struct Rate {
    num: u64,
    per: Duration,
}

impl Rate {
    /// Create a new rate.
    ///
    /// # Panics
    ///
    /// This function panics if `num` or `per` is 0.
    pub fn new(num: u64, per: Duration) -> Self {
        assert!(num > 0);
        assert!(per > Duration::from_millis(0));

        Rate { num, per }
    }

    pub(crate) fn num(&self) -> u64 {
        self.num
    }

    pub(crate) fn per(&self) -> Duration {
        self.per
    }
}

#[derive(Clone, Debug)]
enum State {
    Limited,
    Ready { until: Instant, rem: u64 },
}

// === impl NewRateLimitPolicy ===

/*impl<N> NewRateLimitPolicy<N> {
    pub fn layer() -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self {
            inner,
        })
    }
}*/

impl<T, N> svc::NewService<T> for NewRateLimitPolicy<N>
where
    T: svc::Param<Remote<ClientAddr>>,
    N: Clone,
{
    type Service = RateLimitPolicyService<N>;

    fn new_service(&self, target: T) -> Self::Service {
        let client = target.param();
        let until = Instant::now();
        let rate = Rate::new(5, Duration::from_secs(10));
        let state = State::Ready {
            until,
            rem: 5,
        };

        RateLimitPolicyService {
            client,
            rate,
            state,
            // The sleep won't actually be used with this duration, but
            // we create it eagerly so that we can reset it in place rather than
            // `Box::pin`ning a new `Sleep` every time we need one.
            sleep: Box::pin(tokio::time::sleep_until(until)),
            inner: self.inner.clone(),
        }
    }
}

// === impl RateLimitPolicyService ===

impl<S: Clone> Clone for RateLimitPolicyService<S> {
    fn clone(&self) -> Self {
        let until = Instant::now();
        let sleep = Box::pin(tokio::time::sleep_until(until));
        Self {
            client: self.client.clone(),
            rate: self.rate.clone(),
            state: self.state.clone(),
            sleep,
            inner: self.inner.clone(),
        }
    }
}

impl<N, Req> svc::Service<Req> for RateLimitPolicyService<N>
where
    N: svc::Service<Req>,
    N::Error: Into<Error>,
{
    type Response = N::Response;
    type Error = Error;
    type Future = future::Either<
        future::MapErr<N::Future, fn(N::Error) -> Error>,
        future::Ready<Result<N::Response, Error>>,
    >;

    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> task::Poll<Result<(), Self::Error>> {
        match self.state {
            State::Ready { .. } => return task::Poll::Ready(futures::ready!(self.inner.poll_ready(cx).map_err(Into::into))),
            State::Limited => {
                if Pin::new(&mut self.sleep).poll(cx).is_pending() {
                    return task::Poll::Ready(Ok(()));
                }
            }
        }

        self.state = State::Ready {
            until: Instant::now() + self.rate.per(),
            rem: self.rate.num(),
        };

        task::Poll::Ready(futures::ready!(self.inner.poll_ready(cx)).map_err(Into::into))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        match self.state {
            State::Ready { mut until, mut rem } => {
                let now = Instant::now();

                // If the period has elapsed, reset it.
                if now >= until {
                    until = now + self.rate.per();
                    rem = self.rate.num();
                }

                if rem > 1 {
                    rem -= 1;
                    self.state = State::Ready { until, rem };
                } else {
                    // The service is disabled until further notice
                    // Reset the sleep future in place, so that we don't have to
                    // deallocate the existing box and allocate a new one.
                    self.sleep.as_mut().reset(until);
                    self.state = State::Limited;
                }

                future::Either::Left(self.inner.call(req).map_err(Into::into))
            }
            State::Limited => {
                tracing::info!("too many requests");
                future::Either::Right(future::err(RateLimitError(()).into()))
            }
        }
    }
}

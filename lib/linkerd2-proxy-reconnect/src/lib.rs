//! Conditionally reconnects with a pluggable recovery/backoff strategy.
#![deny(warnings, rust_2018_idioms)]

use futures::{future, try_ready, Poll, Stream};
use linkerd2_never::Never;
use linkerd2_proxy_core::{Error, Recover};
use std::marker::PhantomData;
use tower_reconnect::Reconnect as TowerReconnect;

#[derive(Debug)]
pub struct Layer<T, Req, R: Recover> {
    recover: R,
    // T & Req are bound so that Layer may make helpful type assertions.
    _marker: PhantomData<fn(T, Req)>,
}

#[derive(Debug)]
pub struct MakeReconnect<Req, R, M> {
    recover: R,
    make_service: M,
    _marker: PhantomData<fn(Req)>,
}

pub struct Service<T, R, M>
where
    R: Recover,
    M: tower::Service<T>,
{
    reconnect: TowerReconnect<M, T>,
    recover: R,
    backoff: Option<R::Backoff>,
}

// === impl Layer ===

pub fn layer<T, Req, R>(recover: R) -> Layer<T, Req, R>
where
    T: Clone,
    R: Recover + Clone,
{
    Layer {
        recover,
        _marker: PhantomData,
    }
}

impl<T, Req, R, M, S> tower::layer::Layer<M> for Layer<T, Req, R>
where
    T: Clone,
    R: Recover + Clone,
    M: tower::Service<T, Response = S> + Clone,
    S: tower::Service<Req>,
    Error: From<M::Error> + From<S::Error>,
{
    type Service = MakeReconnect<Req, R, M>;

    fn layer(&self, make_service: M) -> Self::Service {
        MakeReconnect {
            make_service,
            recover: self.recover.clone(),
            _marker: PhantomData,
        }
    }
}

// === impl MakeReconnect ===

impl<Req, T, R, M, S> tower::Service<T> for MakeReconnect<Req, R, M>
where
    T: Clone,
    R: Recover + Clone,
    M: tower::Service<T, Response = S> + Clone,
    S: tower::Service<Req>,
    Error: From<M::Error> + From<S::Error>,
{
    type Response = Service<T, R, M>;
    type Error = Never;
    type Future = future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    fn call(&mut self, target: T) -> Self::Future {
        let reconnect = TowerReconnect::new(self.make_service.clone(), target.clone());
        future::ok(Service {
            reconnect,
            recover: self.recover.clone(),
            backoff: None,
        })
    }
}

impl<Req, R, M> Clone for MakeReconnect<Req, R, M>
where
    R: Recover + Clone,
    M: Clone,
{
    fn clone(&self) -> Self {
        Self {
            recover: self.recover.clone(),
            make_service: self.make_service.clone(),
            _marker: PhantomData,
        }
    }
}

// === impl Service ===

impl<Req, T, R, M, S> tower::Service<Req> for Service<T, R, M>
where
    T: Clone,
    R: Recover,
    M: tower::Service<T, Response = S>,
    S: tower::Service<Req>,
    Error: From<M::Error> + From<S::Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = <TowerReconnect<M, T> as tower::Service<Req>>::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        loop {
            if let Some(backoff) = self.backoff.as_mut() {
                if try_ready!(backoff.poll().map_err(Into::into)).is_none() {
                    self.backoff = None;
                }
                tracing::debug!("Reconnecting");
            }

            match self.reconnect.poll_ready() {
                Ok(ready) => {
                    if ready.is_ready() {
                        if self.backoff.take().is_some() {
                            tracing::debug!("Reconnected");
                        }
                    }
                    return Ok(ready);
                }
                Err(err) => {
                    // Ensure the most recent error is not fatal. Either
                    // continue using the existing backoff or use the new one.
                    let msg = err.to_string();
                    let new_backoff = self.recover.recover(err)?;
                    if self.backoff.is_none() {
                        tracing::warn!("Connection failed: {}", msg);
                        self.backoff = Some(new_backoff);
                    } else {
                        tracing::debug!("Connection failed: {}", msg);
                    }
                }
            }
        }
    }

    #[inline]
    fn call(&mut self, request: Req) -> Self::Future {
        self.reconnect.call(request)
    }
}

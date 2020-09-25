#![deny(warnings, rust_2018_idioms)]

use linkerd2_addr::Addr;
use linkerd2_error::Error;
use std::{
    future::Future,
    task::{Context, Poll},
};
use tower::util::{Oneshot, ServiceExt};

mod client;
pub mod discover;
pub mod http;
pub mod split;

pub use self::client::Client;

pub type Receiver = tokio::sync::watch::Receiver<Profile>;

#[derive(Clone, Debug, Default)]
pub struct Profile {
    pub http_routes: Vec<(self::http::RequestMatch, self::http::Route)>,
    pub targets: Vec<Target>,
}

#[derive(Clone, Debug)]
pub struct Target {
    pub addr: Addr,
    pub weight: u32,
}

#[derive(Clone, Debug)]
pub struct GetProfileService<P>(P);

/// Watches a destination's Profile.
pub trait GetProfile<T> {
    type Error: Into<Error>;
    type Future: Future<Output = Result<Receiver, Self::Error>>;

    fn get_profile(&mut self, target: T) -> Self::Future;

    fn into_service(self) -> GetProfileService<Self>
    where
        Self: Sized,
    {
        GetProfileService(self)
    }
}

impl<T, S> GetProfile<T> for S
where
    S: tower::Service<T, Response = Receiver> + Clone,
    S::Error: Into<Error>,
{
    type Error = S::Error;
    type Future = Oneshot<S, T>;

    fn get_profile(&mut self, target: T) -> Self::Future {
        self.clone().oneshot(target)
    }
}

impl<T, P> tower::Service<T> for GetProfileService<P>
where
    P: GetProfile<T>,
{
    type Response = Receiver;
    type Error = P::Error;
    type Future = P::Future;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        self.0.get_profile(target)
    }
}

#![deny(warnings, rust_2018_idioms)]

use linkerd2_addr::Addr;
use linkerd2_error::Error;
use std::{
    future::Future,
    task::{Context, Poll},
};

mod client;
pub mod discover;
pub mod http;
pub mod split;

pub use self::client::{Client, InvalidProfileAddr};

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

/// Watches a destination's Routes.
///
/// The stream updates with all routes for the given destination. The stream
/// never ends and cannot fail.
pub trait GetProfile<T> {
    type Error: Into<Error>;
    type Future: Future<Output = Result<Receiver, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>>;

    fn get_routes(&mut self, target: T) -> Self::Future;
}

impl<T, S> GetProfile<T> for S
where
    S: tower::Service<T, Response = Receiver>,
    S::Error: Into<Error>,
{
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        tower::Service::poll_ready(self, cx)
    }

    fn get_routes(&mut self, target: T) -> Self::Future {
        tower::Service::call(self, target)
    }
}

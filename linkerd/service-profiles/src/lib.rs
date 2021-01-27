#![deny(warnings, rust_2018_idioms)]

use futures::stream::Stream;
use linkerd_addr::Addr;
pub use linkerd_dns_name::Name;
use linkerd_error::Error;
use linkerd_proxy_api_resolve::Metadata;
use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tower::util::{Oneshot, ServiceExt};

mod client;
mod default;
pub mod discover;
pub mod http;
pub mod split;

pub use self::client::Client;

pub type Receiver = tokio::sync::watch::Receiver<Profile>;

#[derive(Clone, Debug, Default)]
pub struct Profile {
    pub name: Option<Name>,
    pub http_routes: Vec<(self::http::RequestMatch, self::http::Route)>,
    pub targets: Vec<Target>,
    pub opaque_protocol: bool,
    pub endpoint: Option<(SocketAddr, Metadata)>,
}

/// A profile lookup target.
#[derive(Clone, Debug)]
pub struct Logical(pub Addr);

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
    type Future: Future<Output = Result<Option<Receiver>, Self::Error>>;

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
    S: tower::Service<T, Response = Option<Receiver>> + Clone,
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
    type Response = Option<Receiver>;
    type Error = P::Error;
    type Future = P::Future;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        self.0.get_profile(target)
    }
}

fn stream_profile(mut rx: Receiver) -> Pin<Box<dyn Stream<Item = Profile> + Send + Sync>> {
    Box::pin(async_stream::stream! {
        loop {
            let val = rx.borrow().clone();
            yield val;
            // This is a loop with a return condition rather than a while loop,
            // because we want to yield the *first* value immediately, rather
            // than waiting for the profile to change again.
            if let Err(_) = rx.changed().await {
                tracing::trace!("profile sender dropped");
                return;
            }
        }
    })
}

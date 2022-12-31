#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub use linkerd_client_policy::{Backend, Backends, LogicalAddr, LookupAddr};
use linkerd_error::Error;
use std::{
    future::Future,
    task::{Context, Poll},
};
use thiserror::Error;
use tower::util::{Oneshot, ServiceExt};

mod client;
mod default;
pub mod discover;
pub mod http;
mod proto;
pub mod tcp;

pub use self::client::Client;

pub type Profile = linkerd_client_policy::Policy<http::RouteSet, tcp::RouteSet>;

pub type Receiver = linkerd_client_policy::Receiver<http::RouteSet, tcp::RouteSet>;
pub type ReceiverStream = linkerd_client_policy::ReceiverStream<http::RouteSet, tcp::RouteSet>;

#[derive(Clone, Debug)]
pub struct GetProfileService<P>(P);

#[derive(Debug, Error)]
pub enum DiscoveryRejected {
    #[error("discovery rejected by control plane: {0}")]
    Remote(
        #[from]
        #[source]
        tonic::Status,
    ),
    #[error("discovery rejected: {0}")]
    Message(&'static str),
}

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

    #[inline]
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

    #[inline]
    fn call(&mut self, target: T) -> Self::Future {
        self.0.get_profile(target)
    }
}

// === impl DiscoveryRejected ===

impl DiscoveryRejected {
    pub fn new(message: &'static str) -> Self {
        Self::Message(message)
    }

    pub fn is_rejected(err: &(dyn std::error::Error + 'static)) -> bool {
        let mut current = Some(err);
        while let Some(err) = current {
            if err.is::<Self>() {
                return true;
            }

            if let Some(status) = err.downcast_ref::<tonic::Status>() {
                let code = status.code();
                return
                    // Address is not resolveable
                    code == tonic::Code::InvalidArgument
                    // Unexpected cluster state
                    || code == tonic::Code::FailedPrecondition;
            }

            current = err.source();
        }
        false
    }
}

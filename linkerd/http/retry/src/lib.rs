#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod replay;
pub mod with_trailers;

pub use self::{replay::ReplayBody, with_trailers::WithTrailers};
pub use tower::retry::{budget::Budget, Policy};

#[cfg(feature = "fixme")]
mod todo {
    use futures::future;
    use linkerd_error::Error;
    use linkerd_http_box::BoxBody;
    use linkerd_stack::{
        layer::{self, Layer},
        proxy::{Proxy, ProxyService},
        util::AndThen,
        Either, NewService, Service,
    };
    use std::{
        future::Future,
        task::{Context, Poll},
    };
    use tracing::trace;

    /// Applies per-target retry policies.
    #[derive(Clone, Debug)]
    pub struct NewRetry<P, O, I, N> {
        new_policy: P,
        inner: N,
        inner_retry_proxy: I,
        outer_retry_proxy: O,
    }

    #[derive(Clone, Debug)]
    pub struct Retry<P, O, I, S> {
        policy: Option<P>,
        inner: S,
        inner_retry_proxy: I,
        outer_retry_proxy: O,
    }

    // === impl NewRetry ===

    // impl<P: Clone, N, O: Clone> NewRetry<P, N, O> {
    //     pub fn layer(new_policy: P, proxy: ) -> impl Layer<N, Service = Self> + Clone {
    //         layer::mk(move |inner| Self {
    //             inner,
    //             new_policy: new_policy.clone(),
    //             inner_retry_proxy: proxy.clone(),
    //         })
    //     }
    // }

    impl<T, N, P, O> NewService<T> for NewRetry<P, N, O>
    where
        N: NewService<T>,
        P: NewPolicy<T>,
        O: Clone,
    {
        type Service = Retry<P::Policy, N::Service, O>;

        fn new_service(&self, target: T) -> Self::Service {
            // Determine if there is a retry policy for the given target.
            let policy = self.new_policy.new_policy(&target);

            let inner = self.inner.new_service(target);
            Retry {
                policy,
                inner,
                proxy: self.proxy.clone(),
            }
        }
    }

    // === impl Retry ===

    // The inner service created for requests that are retryable.
    type RetrySvc<P, I, S> = tower::retry::Retry<P, ProxyService<I, S>>;

    impl<Req, P, O, I, S, Rsp> Service<http::Request<BoxBody>> for Retry<P, O, I, S>
    where
        P: Policy<O::Request, Rsp, Error> + Clone + std::fmt::Debug,
        O: Proxy<P::RetryRequest, RetrySvc<P, S, Rsp, S>, Error = Error>,
        O: Clone,
        I: Proxy<O::Request, S, Error = Error>,
        I: Clone,
        S: Service<Req, Error = Error>,
        S: Service<I::Request, Error = Error>,
        S: Clone,
    {
        type Response = O::Response;
        type Error = Error;
        type Future = future::Either<<S as Service<Req>>::Future, O::Future>;

        #[inline]
        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            <S as Service<Req>>::poll_ready(&mut self.inner, cx)
        }

        fn call(&mut self, req: http::Request<BoxBody>) -> Self::Future {
            let (policy, req) = match self.policy.clone() {
                Some(p) => match p.prepare_request(req) {
                    Either::A(req) => req,
                    Either::B(req) => return future::Either::Left(self.inner.call(req)),
                },
                None => return future::Either::Left(self.inner.call(req)),
            };
            trace!(retryable = true, ?policy);

            // Take the inner service, replacing it with a clone. This allows the
            // readiness from poll_ready
            let pending = self.inner.clone();
            let ready = std::mem::replace(&mut self.inner, pending);

            let inner = self.inner_retry_proxy.clone().into_service(ready);

            // Retry::poll_ready is just a pass-through to the inner service, so we
            // can rely on the fact that we've taken the ready inner service handle.
            let mut svc = tower::retry::Retry::new(policy, inner);

            future::Either::Right(self.outer_retry_proxy.proxy(&mut svc, svc))
        }
    }
}

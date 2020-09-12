use crate::proxy::http::{orig_proto, settings::Settings};
use crate::svc::stack;
use crate::HttpEndpoint;
use futures::{ready, TryFuture};
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::util::Either;
use tracing::trace;

#[derive(Clone, Debug, Default)]
pub struct OrigProtoUpgradeLayer(());

#[derive(Clone, Debug)]
pub struct OrigProtoUpgrade<M> {
    inner: M,
}

#[pin_project]
pub struct UpgradeFuture<F> {
    can_upgrade: bool,
    #[pin]
    inner: F,
}

impl OrigProtoUpgradeLayer {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<M> tower::layer::Layer<M> for OrigProtoUpgradeLayer {
    type Service = OrigProtoUpgrade<M>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service { inner }
    }
}

// === impl OrigProtoUpgrade ===

impl<N> stack::NewService<HttpEndpoint> for OrigProtoUpgrade<N>
where
    N: stack::NewService<HttpEndpoint>,
{
    type Service = Either<orig_proto::Upgrade<N::Service>, N::Service>;

    fn new_service(&self, mut endpoint: HttpEndpoint) -> Self::Service {
        if !endpoint.can_use_orig_proto() {
            trace!("Endpoint does not support transparent HTTP/2 upgrades");
            return Either::B(self.inner.new_service(endpoint));
        }

        trace!(
            header = %orig_proto::L5D_ORIG_PROTO,
            "Endpoint supports transparent HTTP/2 upgrades",
        );
        endpoint.settings = Settings::Http2;

        let inner = self.inner.new_service(endpoint);
        Either::A(orig_proto::Upgrade::new(inner))
    }
}

impl<M> tower::Service<HttpEndpoint> for OrigProtoUpgrade<M>
where
    M: tower::Service<HttpEndpoint>,
{
    type Response = Either<orig_proto::Upgrade<M::Response>, M::Response>;
    type Error = M::Error;
    type Future = UpgradeFuture<M::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut endpoint: HttpEndpoint) -> Self::Future {
        let can_upgrade = endpoint.can_use_orig_proto();

        if can_upgrade {
            trace!(
                header = %orig_proto::L5D_ORIG_PROTO,
                "Endpoint supports transparent HTTP/2 upgrades",
            );
            endpoint.settings = Settings::Http2;
        }

        let inner = self.inner.call(endpoint);
        UpgradeFuture { can_upgrade, inner }
    }
}

// === impl UpgradeFuture ===

impl<F> Future for UpgradeFuture<F>
where
    F: TryFuture,
{
    type Output = Result<Either<orig_proto::Upgrade<F::Ok>, F::Ok>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let inner = ready!(this.inner.try_poll(cx))?;

        if *this.can_upgrade {
            let upgrade = orig_proto::Upgrade::new(inner);
            Poll::Ready(Ok(Either::A(upgrade)))
        } else {
            Poll::Ready(Ok(Either::B(inner)))
        }
    }
}

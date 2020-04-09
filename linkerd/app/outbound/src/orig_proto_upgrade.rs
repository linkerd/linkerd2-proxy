use crate::proxy::http::overwrite_authority::ShouldOverwriteAuthority;
use crate::proxy::http::{orig_proto, settings::Settings};
use crate::svc::stack;
use crate::{HttpEndpoint, Target};
use futures::{try_ready, Future, Poll};
use tower::util::Either;
use tracing::trace;

#[derive(Clone, Debug, Default)]
pub struct OrigProtoUpgradeLayer(());

#[derive(Clone, Debug)]
pub struct OrigProtoUpgrade<M> {
    inner: M,
}

pub struct UpgradeFuture<F> {
    can_upgrade: bool,
    inner: F,
    was_absolute: bool,
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

impl<N> stack::NewService<Target<HttpEndpoint>> for OrigProtoUpgrade<N>
where
    N: stack::NewService<Target<HttpEndpoint>>,
{
    type Service = Either<orig_proto::Upgrade<N::Service>, N::Service>;

    fn new_service(&self, mut endpoint: Target<HttpEndpoint>) -> Self::Service {
        if !endpoint.inner.can_use_orig_proto() {
            trace!("Endpoint does not support transparent HTTP/2 upgrades");
            return Either::B(self.inner.new_service(endpoint));
        }

        let was_absolute =
            endpoint.should_overwrite_authority() || endpoint.inner.settings.was_absolute_form();

        trace!(
            header = %orig_proto::L5D_ORIG_PROTO,
            was_absolute,
            "Endpoint supports transparent HTTP/2 upgrades",
        );
        endpoint.inner.settings = Settings::Http2;

        let inner = self.inner.new_service(endpoint);
        Either::A(orig_proto::Upgrade::new(inner, was_absolute))
    }
}

impl<M> tower::Service<Target<HttpEndpoint>> for OrigProtoUpgrade<M>
where
    M: tower::Service<Target<HttpEndpoint>>,
{
    type Response = Either<orig_proto::Upgrade<M::Response>, M::Response>;
    type Error = M::Error;
    type Future = UpgradeFuture<M::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut endpoint: Target<HttpEndpoint>) -> Self::Future {
        let can_upgrade = endpoint.inner.can_use_orig_proto();

        let was_absolute =
            endpoint.should_overwrite_authority() || endpoint.inner.settings.was_absolute_form();

        if can_upgrade {
            trace!(
                header = %orig_proto::L5D_ORIG_PROTO,
                %was_absolute,
                "Endpoint supports transparent HTTP/2 upgrades",
            );
            endpoint.inner.settings = Settings::Http2;
        }

        let inner = self.inner.call(endpoint);
        UpgradeFuture {
            can_upgrade,
            inner,
            was_absolute,
        }
    }
}

// === impl UpgradeFuture ===

impl<F> Future for UpgradeFuture<F>
where
    F: Future,
{
    type Item = Either<orig_proto::Upgrade<F::Item>, F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());

        if self.can_upgrade {
            let upgrade = orig_proto::Upgrade::new(inner, self.was_absolute);
            Ok(Either::A(upgrade).into())
        } else {
            Ok(Either::B(inner).into())
        }
    }
}

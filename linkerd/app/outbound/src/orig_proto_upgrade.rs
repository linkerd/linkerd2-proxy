use crate::endpoint::Endpoint;
use crate::proxy::http::{orig_proto, settings::Settings};
use crate::svc;
use futures::{try_ready, Future, Poll};
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

impl<M> svc::Layer<M> for OrigProtoUpgradeLayer {
    type Service = OrigProtoUpgrade<M>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service { inner }
    }
}

// === impl OrigProtoUpgrade ===

impl<N> svc::NewService<Endpoint> for OrigProtoUpgrade<N>
where
    N: svc::NewService<Endpoint>,
{
    type Service = svc::Either<orig_proto::Upgrade<N::Service>, N::Service>;

    fn new_service(&self, mut endpoint: Endpoint) -> Self::Service {
        if !endpoint.can_use_orig_proto() {
            trace!("Endpoint does not support transparent HTTP/2 upgrades");
            return svc::Either::B(self.inner.new_service(endpoint));
        }

        let was_absolute = endpoint.http_settings.was_absolute_form();
        trace!(
            header = %orig_proto::L5D_ORIG_PROTO,
            %was_absolute,
            "Endpoint supports transparent HTTP/2 upgrades",
        );
        endpoint.http_settings = Settings::Http2;

        let mut upgrade = orig_proto::Upgrade::new(self.inner.new_service(endpoint));
        upgrade.absolute_form = was_absolute;
        svc::Either::A(upgrade)
    }
}

impl<M> svc::Service<Endpoint> for OrigProtoUpgrade<M>
where
    M: svc::Service<Endpoint>,
{
    type Response = svc::Either<orig_proto::Upgrade<M::Response>, M::Response>;
    type Error = M::Error;
    type Future = UpgradeFuture<M::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut endpoint: Endpoint) -> Self::Future {
        let can_upgrade = endpoint.can_use_orig_proto();

        let was_absolute = endpoint.http_settings.was_absolute_form();
        if can_upgrade {
            trace!(
                header = %orig_proto::L5D_ORIG_PROTO,
                %was_absolute,
                "Endpoint supports transparent HTTP/2 upgrades",
            );
            endpoint.http_settings = Settings::Http2;
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
    type Item = svc::Either<orig_proto::Upgrade<F::Item>, F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());

        if self.can_upgrade {
            let mut upgrade = orig_proto::Upgrade::new(inner);
            upgrade.absolute_form = self.was_absolute;
            Ok(svc::Either::A(upgrade).into())
        } else {
            Ok(svc::Either::B(inner).into())
        }
    }
}

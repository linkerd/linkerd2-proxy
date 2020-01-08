use super::HttpEndpoint;
use crate::proxy::http::{orig_proto, settings::Settings};
use crate::svc;
use futures::{try_ready, Future, Poll};
use tracing::trace;

#[derive(Clone, Debug)]
pub struct Layer(());

#[derive(Clone, Debug)]
pub struct MakeSvc<M> {
    inner: M,
}

pub struct MakeFuture<F> {
    can_upgrade: bool,
    inner: F,
    was_absolute: bool,
}

pub fn layer() -> Layer {
    Layer(())
}

impl<M> svc::Layer<M> for Layer {
    type Service = MakeSvc<M>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeSvc { inner }
    }
}

// === impl MakeSvc ===

impl<N> svc::NewService<HttpEndpoint> for MakeSvc<N>
where
    N: svc::NewService<HttpEndpoint>,
{
    type Service = svc::Either<orig_proto::Upgrade<N::Service>, N::Service>;

    fn new_service(&self, mut endpoint: HttpEndpoint) -> Self::Service {
        if !endpoint.can_use_orig_proto() {
            trace!("Endpoint does not support transparent HTTP/2 upgrades");
            return svc::Either::B(self.inner.new_service(endpoint));
        }

        let was_absolute = endpoint.concrete.logical.settings.was_absolute_form();
        trace!(
            header = %orig_proto::L5D_ORIG_PROTO,
            %was_absolute,
            "Endpoint supports transparent HTTP/2 upgrades",
        );
        endpoint.concrete.logical.settings = Settings::Http2;

        let mut upgrade = orig_proto::Upgrade::new(self.inner.new_service(endpoint));
        upgrade.absolute_form = was_absolute;
        svc::Either::A(upgrade)
    }
}

impl<M> svc::Service<HttpEndpoint> for MakeSvc<M>
where
    M: svc::Service<HttpEndpoint>,
{
    type Response = svc::Either<orig_proto::Upgrade<M::Response>, M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut endpoint: HttpEndpoint) -> Self::Future {
        let can_upgrade = endpoint.can_use_orig_proto();

        let was_absolute = endpoint.concrete.logical.settings.was_absolute_form();
        if can_upgrade {
            trace!(
                header = %orig_proto::L5D_ORIG_PROTO,
                %was_absolute,
                "Endpoint supports transparent HTTP/2 upgrades",
            );
            endpoint.concrete.logical.settings = Settings::Http2;
        }

        let inner = self.inner.call(endpoint);
        MakeFuture {
            can_upgrade,
            inner,
            was_absolute,
        }
    }
}

// === impl MakeFuture ===

impl<F> Future for MakeFuture<F>
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

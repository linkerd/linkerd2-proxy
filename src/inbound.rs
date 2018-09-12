use std::net::{SocketAddr};
use std::sync::Arc;

use tower_service as tower;
use tower_buffer::Buffer;
use tower_in_flight_limit::InFlightLimit;
use tower_h2;

use bind;
use ctx;
use proxy;

type Bind<B> = bind::Bind<ctx::Proxy, B>;

pub struct Inbound<B> {
    default_addr: Option<SocketAddr>,
    bind: Bind<B>,
}

const MAX_IN_FLIGHT: usize = 10_000;

// ===== impl Inbound =====

impl<B> Inbound<B> {
    pub fn new(default_addr: Option<SocketAddr>, bind: Bind<B>) -> Self {
        Self {
            default_addr,
            bind,
        }
    }
}

impl<B> Clone for Inbound<B>
where
    B: tower_h2::Body + 'static,
{
    fn clone(&self) -> Self {
        Self {
            bind: self.bind.clone(),
            default_addr: self.default_addr.clone(),
        }
    }
}

impl<B> proxy::http::router::Recognize for Inbound<B>
where
    B: tower_h2::Body + Send + 'static,
    <B::Data as ::bytes::IntoBuf>::Buf: Send,
{
    type Key = (SocketAddr, proxy::http::Dialect);
    type Request = <Self::Service as tower::Service>::Request;
    type Response = <Self::Service as tower::Service>::Response;
    type Error = <Self::Service as tower::Service>::Error;
    type RouteError = bind::BufferSpawnError;
    type Service = InFlightLimit<Buffer<
        proxy::http::orig_proto::Downgrade<bind::BoundService<B>>,
    >>;

    fn recognize(&self, req: &Self::Request) -> Option<Self::Key> {
        let key = req.extensions()
            .get::<Arc<ctx::transport::Server>>()
            .and_then(|ctx| {
                trace!("recognize local={} orig={:?}", ctx.local, ctx.orig_dst);
                ctx.orig_dst_if_not_local()
            })
            .or_else(|| self.default_addr);

        let proto = proxy::http::orig_proto::detect(req);

        let key = key.map(move |addr| (addr, proto));
        trace!("recognize key={:?}", key);

        key
    }

    /// Builds a static service to a single endpoint.
    ///
    /// # TODO
    ///
    /// Buffering does not apply timeouts.
    fn bind_service(&self, key: &Self::Key) -> Result<Self::Service, Self::RouteError> {
        let &(ref addr, ref proto) = key;
        debug!("building inbound {:?} client to {}", proto, addr);

        let endpoint = (*addr).into();
        let binding = self.bind.bind_service(&endpoint, proto);
        let from_orig_proto = proxy::http::orig_proto::Downgrade::new(binding);

        let log = ::logging::proxy().client("in", "local")
            .with_remote(*addr);
        Buffer::new(from_orig_proto, &log.executor())
            .map(|buffer| {
                InFlightLimit::new(buffer, MAX_IN_FLIGHT)
            })
            .map_err(|_| bind::BufferSpawnError::Inbound)
    }
}

#[cfg(test)]
mod tests {
    use std::net;

    use http;

    use super::Inbound;
    use bind::Bind;
    use ctx;
    use conditional::Conditional;
    use proxy;
    use proxy::http::router::Recognize;
    use tls;

    fn new_inbound(default: Option<net::SocketAddr>, ctx: ctx::Proxy) -> Inbound<()> {
        let bind = Bind::new(
            ::telemetry::Sensors::for_test(),
            ::transport::metrics::Registry::default(),
            tls::ClientConfig::no_tls()
        );
        Inbound::new(default, bind.with_ctx(ctx))
    }

    fn make_key_http1(addr: net::SocketAddr) -> (net::SocketAddr, proxy::http::Dialect) {
        let dialect = proxy::http::Dialect::Http1 {
            host: proxy::http::dialect::Host::NoAuthority,
            is_h1_upgrade: false,
            was_absolute_form: false,
        };
        (addr, dialect)
    }

    const TLS_DISABLED: Conditional<(), tls::ReasonForNoTls> =
        Conditional::None(tls::ReasonForNoTls::Disabled);

    quickcheck! {
        fn recognize_orig_dst(
            orig_dst: net::SocketAddr,
            local: net::SocketAddr,
            remote: net::SocketAddr
        ) -> bool {
            let ctx = ctx::Proxy::Inbound;

            let inbound = new_inbound(None, ctx);

            let srv_ctx = ctx::transport::Server::new(
                ctx, &local, &remote, &Some(orig_dst), TLS_DISABLED);

            let rec = srv_ctx.orig_dst_if_not_local().map(make_key_http1);

            let mut req = http::Request::new(());
            req.extensions_mut()
                .insert(srv_ctx);

            inbound.recognize(&req) == rec
        }

        fn recognize_default_no_orig_dst(
            default: Option<net::SocketAddr>,
            local: net::SocketAddr,
            remote: net::SocketAddr
        ) -> bool {
            let ctx = ctx::Proxy::Inbound;

            let inbound = new_inbound(default, ctx);

            let mut req = http::Request::new(());
            req.extensions_mut()
                .insert(ctx::transport::Server::new(
                    ctx,
                    &local,
                    &remote,
                    &None,
                    TLS_DISABLED,
                ));

            inbound.recognize(&req) == default.map(make_key_http1)
        }

        fn recognize_default_no_ctx(default: Option<net::SocketAddr>) -> bool {
            let ctx = ctx::Proxy::Inbound;

            let inbound = new_inbound(default, ctx);

            let req = http::Request::new(());

            inbound.recognize(&req) == default.map(make_key_http1)
        }

        fn recognize_default_no_loop(
            default: Option<net::SocketAddr>,
            local: net::SocketAddr,
            remote: net::SocketAddr
        ) -> bool {
            let ctx = ctx::Proxy::Inbound;

            let inbound = new_inbound(default, ctx);

            let mut req = http::Request::new(());
            req.extensions_mut()
                .insert(ctx::transport::Server::new(
                    ctx,
                    &local,
                    &remote,
                    &Some(local),
                    TLS_DISABLED,
                ));

            inbound.recognize(&req) == default.map(make_key_http1)
        }
    }
}

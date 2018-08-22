use std::net::{SocketAddr};
use std::sync::Arc;

use http;
use tower_service as tower;
use tower_buffer::{self, Buffer};
use tower_in_flight_limit::{self, InFlightLimit};
use tower_h2;
use linkerd2_proxy_router::Recognize;

use bind;
use ctx;
use transparency::orig_proto;

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

impl<B> Recognize for Inbound<B>
where
    B: tower_h2::Body + Send + 'static,
    <B::Data as ::bytes::IntoBuf>::Buf: Send,
{
    type Request = http::Request<B>;
    type Response = bind::HttpResponse;
    type Error = tower_in_flight_limit::Error<
        tower_buffer::Error<
            <bind::Service<B> as tower::Service>::Error
        >
    >;
    type Key = (SocketAddr, bind::Protocol);
    type RouteError = bind::BufferSpawnError;
    type Service = InFlightLimit<Buffer<orig_proto::Downgrade<bind::Service<B>>>>;

    fn recognize(&self, req: &Self::Request) -> Option<Self::Key> {
        let key = req.extensions()
            .get::<Arc<ctx::transport::Server>>()
            .and_then(|ctx| {
                trace!("recognize local={} orig={:?}", ctx.local, ctx.orig_dst);
                ctx.orig_dst_if_not_local()
            })
            .or_else(|| self.default_addr);

        let proto = orig_proto::detect(req);

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
        let from_orig_proto = orig_proto::Downgrade::new(binding);

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
    use linkerd2_proxy_router::Recognize;

    use super::Inbound;
    use bind::{self, Bind, Host};
    use ctx;
    use conditional::Conditional;
    use tls;

    fn new_inbound(default: Option<net::SocketAddr>, ctx: ctx::Proxy) -> Inbound<()> {
        let bind = Bind::new(
            ::telemetry::Sensors::for_test(),
            ::telemetry::transport::Registry::default(),
            tls::ClientConfig::no_tls()
        );
        Inbound::new(default, bind.with_ctx(ctx))
    }

    fn make_key_http1(addr: net::SocketAddr) -> (net::SocketAddr, bind::Protocol) {
        let protocol = bind::Protocol::Http1 {
            host: Host::NoAuthority,
            is_h1_upgrade: false,
            was_absolute_form: false,
        };
        (addr, protocol)
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

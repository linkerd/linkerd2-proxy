use crate::{
    classify, config, dns, identity, metrics, proxy::http, svc, tls, transport::ConnectTcp, Addr,
    Error,
};
use futures::future::Either;
use std::fmt;
use tokio::time;
use tokio_stream::{wrappers::IntervalStream, StreamExt};
use tracing::{info_span, warn};

#[derive(Clone, Debug)]
pub struct Config {
    pub addr: ControlAddr,
    pub connect: config::ConnectConfig,
    pub buffer: config::QueueConfig,
}

#[derive(Clone, Debug)]
pub struct ControlAddr {
    pub addr: Addr,
    pub identity: tls::ConditionalClientTls,
}

impl svc::Param<Addr> for ControlAddr {
    fn param(&self) -> Addr {
        self.addr.clone()
    }
}

impl svc::Param<http::balance::EwmaConfig> for ControlAddr {
    fn param(&self) -> http::balance::EwmaConfig {
        EWMA_CONFIG
    }
}

impl fmt::Display for ControlAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.addr, f)
    }
}

pub type RspBody =
    linkerd_http_metrics::requests::ResponseBody<http::balance::Body<hyper::Body>, classify::Eos>;

const EWMA_CONFIG: http::balance::EwmaConfig = http::balance::EwmaConfig {
    default_rtt: time::Duration::from_millis(30),
    decay: time::Duration::from_secs(10),
};

impl Config {
    pub fn build(
        self,
        dns: dns::Resolver,
        metrics: metrics::ControlHttp,
        identity: identity::NewClient,
    ) -> svc::ArcNewService<
        (),
        impl svc::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<RspBody>,
                Error = Error,
                Future = impl Send,
            > + Clone,
    > {
        let addr = self.addr;

        // When a DNS resolution fails, log the error and use the TTL, if there
        // is one, to drive re-resolution attempts.
        let resolve_backoff = {
            let backoff = self.connect.backoff;
            move |error: Error| {
                warn!(error, "Failed to resolve control-plane component");
                if let Some(e) = crate::errors::cause_ref::<dns::ResolveError>(&*error) {
                    if let Some(ttl) = e.negative_ttl() {
                        return Ok(Either::Left(
                            IntervalStream::new(time::interval(ttl)).map(|_| ()),
                        ));
                    }
                }

                // If the error didn't give us a TTL, use the default jittered
                // backoff.
                Ok(Either::Right(backoff.stream()))
            }
        };

        svc::stack(ConnectTcp::new(self.connect.keepalive))
            .push(tls::Client::layer(identity))
            .push_connect_timeout(self.connect.timeout)
            .push_map_target(|(_version, target)| target)
            .push(self::client::layer())
            .push_on_service(svc::MapErr::layer(Into::into))
            .into_new_service()
            // Ensure that connection is driven independently of the load
            // balancer; but don't drive reconnection independently of the
            // balancer. This ensures that new connections are only initiated
            // when the balancer tries to move pending endpoints to ready (i.e.
            // after checking for discovery updates); but we don't want to
            // continually reconnect without checking for discovery updates.
            .push_on_service(svc::layer::mk(svc::SpawnReady::new))
            .push_new_reconnect(self.connect.backoff)
            .instrument(|t: &self::client::Target| info_span!("endpoint", addr = %t.addr))
            .lift_new()
            .push(self::balance::layer(dns, resolve_backoff))
            .push(metrics.to_layer::<classify::Response, _, _>())
            .push(self::add_origin::layer())
            // This buffer allows a resolver client to be shared across stacks.
            // No load shed is applied here, however, so backpressure may leak
            // into the caller task.
            .push(svc::NewQueue::layer_fixed(self.buffer))
            .instrument(|c: &ControlAddr| info_span!("controller", addr = %c.addr))
            .push_map_target(move |()| addr.clone())
            .push(svc::ArcNewService::layer())
            .into_inner()
    }
}

/// Sets the request's URI from `Config`.
mod add_origin {
    use super::ControlAddr;
    use linkerd_stack::{layer, NewService};
    use std::task::{Context, Poll};

    pub fn layer<M>() -> impl layer::Layer<M, Service = NewAddOrigin<M>> + Clone {
        layer::mk(|inner| NewAddOrigin { inner })
    }

    #[derive(Clone, Debug)]
    pub struct NewAddOrigin<N> {
        inner: N,
    }

    #[derive(Clone, Debug)]
    pub struct AddOrigin<S> {
        authority: http::uri::Authority,
        inner: S,
    }

    // === impl NewAddOrigin ===

    impl<N: NewService<ControlAddr>> NewService<ControlAddr> for NewAddOrigin<N> {
        type Service = AddOrigin<N::Service>;

        fn new_service(&self, target: ControlAddr) -> Self::Service {
            AddOrigin {
                authority: target.addr.to_http_authority(),
                inner: self.inner.new_service(target),
            }
        }
    }

    // === AddOrigin ===

    impl<B, S: tower::Service<http::Request<B>>> tower::Service<http::Request<B>> for AddOrigin<S> {
        type Response = S::Response;
        type Error = S::Error;
        type Future = S::Future;

        #[inline]
        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            let (mut parts, body) = req.into_parts();
            parts.uri = {
                let mut uri = parts.uri.into_parts();
                uri.scheme = Some(http::uri::Scheme::HTTP);
                uri.authority = Some(self.authority.clone());
                http::Uri::from_parts(uri).expect("URI must be valid")
            };
            self.inner.call(http::Request::from_parts(parts, body))
        }
    }
}

mod balance {
    use super::{client::Target, ControlAddr};
    use crate::{
        dns,
        proxy::{dns_resolve::DnsResolve, http, resolve::recover},
        svc, tls,
    };
    use std::net::SocketAddr;

    pub fn layer<B, R: Clone, N>(
        dns: dns::Resolver,
        recover: R,
    ) -> impl svc::Layer<
        N,
        Service = http::NewBalancePeakEwma<B, recover::Resolve<R, DnsResolve>, NewIntoTarget<N>>,
    > {
        let resolve = recover::Resolve::new(recover, DnsResolve::new(dns));
        svc::layer::mk(move |inner| {
            http::NewBalancePeakEwma::new(NewIntoTarget { inner }, resolve.clone())
        })
    }

    #[derive(Clone, Debug)]
    pub struct NewIntoTarget<N> {
        inner: N,
    }

    #[derive(Clone, Debug)]
    pub struct IntoTarget<N> {
        inner: N,
        server_id: tls::ConditionalClientTls,
    }

    // === impl NewIntoTarget ===

    impl<N: svc::NewService<ControlAddr>> svc::NewService<ControlAddr> for NewIntoTarget<N> {
        type Service = IntoTarget<N::Service>;

        fn new_service(&self, control: ControlAddr) -> Self::Service {
            IntoTarget {
                server_id: control.identity.clone(),
                inner: self.inner.new_service(control),
            }
        }
    }

    // === impl IntoTarget ===

    impl<N: svc::NewService<Target>> svc::NewService<(SocketAddr, ())> for IntoTarget<N> {
        type Service = N::Service;

        fn new_service(&self, (addr, ()): (SocketAddr, ())) -> Self::Service {
            self.inner
                .new_service(Target::new(addr, self.server_id.clone()))
        }
    }
}

/// Creates a client suitable for gRPC.
mod client {
    use crate::{
        proxy::http,
        svc, tls,
        transport::{Remote, ServerAddr},
    };
    use linkerd_proxy_http::h2::Settings as H2Settings;
    use std::{
        net::SocketAddr,
        task::{Context, Poll},
    };

    #[derive(Clone, Hash, Debug, Eq, PartialEq)]
    pub struct Target {
        pub(super) addr: SocketAddr,
        server_id: tls::ConditionalClientTls,
    }

    impl Target {
        pub(super) fn new(addr: SocketAddr, server_id: tls::ConditionalClientTls) -> Self {
            Self { addr, server_id }
        }
    }

    #[derive(Debug)]
    pub struct Client<C, B> {
        inner: http::h2::Connect<C, B>,
    }

    // === impl Target ===

    impl svc::Param<Remote<ServerAddr>> for Target {
        fn param(&self) -> Remote<ServerAddr> {
            Remote(ServerAddr(self.addr))
        }
    }

    impl svc::Param<SocketAddr> for Target {
        fn param(&self) -> SocketAddr {
            self.addr
        }
    }

    impl svc::Param<tls::ConditionalClientTls> for Target {
        fn param(&self) -> tls::ConditionalClientTls {
            self.server_id.clone()
        }
    }

    // === impl Layer ===

    pub fn layer<C, B>() -> impl svc::Layer<C, Service = Client<C, B>> + Copy
    where
        http::h2::Connect<C, B>: tower::Service<Target>,
    {
        svc::layer::mk(|mk_conn| {
            let inner = http::h2::Connect::new(mk_conn, H2Settings::default());
            Client { inner }
        })
    }

    // === impl Client ===

    impl<C, B> tower::Service<Target> for Client<C, B>
    where
        http::h2::Connect<C, B>: tower::Service<Target>,
    {
        type Response = <http::h2::Connect<C, B> as tower::Service<Target>>::Response;
        type Error = <http::h2::Connect<C, B> as tower::Service<Target>>::Error;
        type Future = <http::h2::Connect<C, B> as tower::Service<Target>>::Future;

        #[inline]
        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

        #[inline]
        fn call(&mut self, target: Target) -> Self::Future {
            self.inner.call(target)
        }
    }

    // A manual impl is needed since derive adds `B: Clone`, but that's just
    // a PhantomData.
    impl<C, B> Clone for Client<C, B>
    where
        http::h2::Connect<C, B>: Clone,
    {
        fn clone(&self) -> Self {
            Client {
                inner: self.inner.clone(),
            }
        }
    }
}

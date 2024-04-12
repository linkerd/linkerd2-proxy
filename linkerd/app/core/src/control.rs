use crate::{
    classify, config, dns, identity, metrics, proxy::http, svc, tls, transport::ConnectTcp, Addr,
    Error,
};
use futures::future::Either;
use linkerd_metrics::prom;
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

#[derive(Debug, thiserror::Error)]
#[error("controller {addr}: {source}")]
pub struct ControlError {
    addr: Addr,
    #[source]
    source: Error,
}

#[derive(Debug, thiserror::Error)]
#[error("endpoint {addr}: {source}")]
struct EndpointError {
    addr: std::net::SocketAddr,
    #[source]
    source: Error,
}

impl svc::Param<Addr> for ControlAddr {
    fn param(&self) -> Addr {
        self.addr.clone()
    }
}

impl svc::Param<svc::queue::Capacity> for ControlAddr {
    fn param(&self) -> svc::queue::Capacity {
        svc::queue::Capacity(1_000)
    }
}

impl svc::Param<svc::queue::Timeout> for ControlAddr {
    fn param(&self) -> svc::queue::Timeout {
        const FAILFAST: time::Duration = time::Duration::from_secs(30);
        svc::queue::Timeout(FAILFAST)
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

#[derive(Clone, Debug, Default)]
pub struct Metrics {
    balance: balance::Metrics,
}

const EWMA_CONFIG: http::balance::EwmaConfig = http::balance::EwmaConfig {
    default_rtt: time::Duration::from_millis(30),
    decay: time::Duration::from_secs(10),
};

impl Metrics {
    pub fn register(registry: &mut prom::Registry) -> Self {
        Metrics {
            balance: balance::Metrics::register(registry.sub_registry_with_prefix("balancer")),
        }
    }
}

impl Config {
    pub fn build(
        self,
        dns: dns::Resolver,
        legacy_metrics: metrics::ControlHttp,
        metrics: Metrics,
        identity: identity::NewClient,
    ) -> svc::ArcNewService<
        (),
        svc::BoxCloneSyncService<http::Request<tonic::body::BoxBody>, http::Response<RspBody>>,
    > {
        let addr = self.addr;
        tracing::trace!(%addr, "Building");

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

        let client = svc::stack(ConnectTcp::new(self.connect.keepalive))
            .push(tls::Client::layer(identity))
            .push_connect_timeout(self.connect.timeout)
            .push_map_target(|(_version, target)| target)
            .push(self::client::layer(self.connect.h2_settings))
            .push_on_service(svc::MapErr::layer_boxed())
            .into_new_service();

        let endpoint = client
            // Ensure that connection is driven independently of the load
            // balancer; but don't drive reconnection independently of the
            // balancer. This ensures that new connections are only initiated
            // when the balancer tries to move pending endpoints to ready (i.e.
            // after checking for discovery updates); but we don't want to
            // continually reconnect without checking for discovery updates.
            .push_on_service(svc::layer::mk(svc::SpawnReady::new))
            .push(svc::NewMapErr::layer_from_target::<EndpointError, _>())
            .push_new_reconnect(self.connect.backoff)
            .instrument(|t: &self::client::Target| info_span!("endpoint", addr = %t.addr));

        let balance = endpoint
            .lift_new()
            .push(self::balance::layer(metrics.balance, dns, resolve_backoff))
            .push(legacy_metrics.to_layer::<classify::Response, _, _>())
            .push(classify::NewClassify::layer_default());

        balance
            .push(self::add_origin::layer())
            .push(svc::NewMapErr::layer_from_target::<ControlError, _>())
            .instrument(|c: &ControlAddr| info_span!("controller", addr = %c.addr))
            .push_map_target(move |()| addr.clone())
            .push_on_service(svc::BoxCloneSyncService::layer())
            .push(svc::ArcNewService::layer())
            .into_inner()
    }
}

impl From<(&ControlAddr, Error)> for ControlError {
    fn from((controller, source): (&ControlAddr, Error)) -> Self {
        Self {
            addr: controller.addr.clone(),
            source,
        }
    }
}

impl From<(&self::client::Target, Error)> for EndpointError {
    fn from((target, source): (&self::client::Target, Error)) -> Self {
        Self {
            addr: target.addr,
            source,
        }
    }
}

/// Sets the request's URI from `Config`.
mod add_origin {
    use super::ControlAddr;
    use crate::svc::{layer, NewService};
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
        metrics::prom::encoding::EncodeLabelSet,
        proxy::{dns_resolve::DnsResolve, http, resolve::recover},
        svc, tls,
    };
    use linkerd_stack::ExtractParam;
    use std::net::SocketAddr;

    pub(super) type Metrics = http::balance::MetricFamilies<Labels>;

    pub fn layer<B, R: Clone, N>(
        metrics: Metrics,
        dns: dns::Resolver,
        recover: R,
    ) -> impl svc::Layer<
        N,
        Service = http::NewBalance<B, Params, recover::Resolve<R, DnsResolve>, NewIntoTarget<N>>,
    > {
        let resolve = recover::Resolve::new(recover, DnsResolve::new(dns));
        svc::layer::mk(move |inner| {
            http::NewBalance::new(
                NewIntoTarget { inner },
                resolve.clone(),
                Params(metrics.clone()),
            )
        })
    }

    #[derive(Clone, Debug)]
    pub struct Params(http::balance::MetricFamilies<Labels>);

    #[derive(Clone, Debug)]
    pub struct NewIntoTarget<N> {
        inner: N,
    }

    #[derive(Clone, Debug)]
    pub struct IntoTarget<N> {
        inner: N,
        server_id: tls::ConditionalClientTls,
    }

    #[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
    pub(super) struct Labels {
        addr: String,
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

    // === impl Metrics ===

    impl ExtractParam<http::balance::Metrics, ControlAddr> for Params {
        fn extract_param(&self, tgt: &ControlAddr) -> http::balance::Metrics {
            self.0.metrics(&Labels {
                addr: tgt.addr.to_string(),
            })
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
    use std::net::SocketAddr;

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

    pub fn layer<C, B>(
        settings: H2Settings,
    ) -> impl svc::Layer<C, Service = http::h2::Connect<C, B>> + Copy
    where
        http::h2::Connect<C, B>: tower::Service<Target>,
    {
        svc::layer::mk(move |mk_conn| http::h2::Connect::new(mk_conn, settings))
    }
}

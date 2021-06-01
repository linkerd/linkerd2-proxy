use crate::{
    classify, config, control, dns, metrics, proxy::http, reconnect, svc, tls,
    transport::ConnectTcp, Addr, Error,
};
use futures::future::Either;
use std::fmt;
use tokio::time;
use tokio_stream::{wrappers::IntervalStream, StreamExt};
use tracing::warn;

#[derive(Clone, Debug)]
pub struct Config {
    pub addr: ControlAddr,
    pub connect: config::ConnectConfig,
    pub buffer_capacity: usize,
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

impl fmt::Display for ControlAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.addr, f)
    }
}

type BalanceBody =
    http::balance::PendingUntilFirstDataBody<tower::load::peak_ewma::Handle, hyper::Body>;

type RspBody = linkerd_http_metrics::requests::ResponseBody<BalanceBody, classify::Eos>;

pub type Client<B> = svc::Buffer<http::Request<B>, http::Response<RspBody>, Error>;

impl Config {
    pub fn build<B, L>(
        self,
        dns: dns::Resolver,
        metrics: metrics::ControlHttp,
        identity: Option<L>,
    ) -> svc::BoxNewService<(), Client<B>>
    where
        B: http::HttpBody + Send + 'static,
        B::Data: Send,
        B::Error: Into<Error> + Send + Sync + 'static,
        L: Clone + svc::Param<tls::client::Config> + Send + Sync + 'static,
    {
        let addr = self.addr;

        let connect_backoff = {
            let backoff = self.connect.backoff;
            move |_| Ok(backoff.stream())
        };

        // When a DNS resolution fails, log the error and use the TTL, if there
        // is one, to drive re-resolution attempts.
        let resolve_backoff = {
            let backoff = self.connect.backoff;
            move |error: Error| {
                warn!(%error, "Failed to resolve control-plane component");
                if let Some(e) = error.downcast_ref::<dns::ResolveError>() {
                    if let dns::ResolveErrorKind::NoRecordsFound {
                        negative_ttl: Some(ttl_secs),
                        ..
                    } = e.kind()
                    {
                        let ttl = time::Duration::from_secs(*ttl_secs as u64);
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
            .push_timeout(self.connect.timeout)
            .push(self::client::layer())
            .push(reconnect::layer(connect_backoff))
            // Ensure individual endpoints are driven to readiness so that the balancer need not
            // drive them all directly.
            .push_on_response(svc::layer::mk(svc::SpawnReady::new))
            .push(self::resolve::layer(dns, resolve_backoff))
            .push_on_response(self::control::balance::layer())
            .into_new_service()
            .push(metrics.to_layer::<classify::Response, _>())
            .push(self::add_origin::layer())
            .push_on_response(svc::layers().push_spawn_buffer(self.buffer_capacity))
            .push_map_target(move |()| addr.clone())
            .push(svc::BoxNewService::layer())
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

        fn new_service(&mut self, target: ControlAddr) -> Self::Service {
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

mod resolve {
    use super::client::Target;
    use crate::{
        dns,
        proxy::{
            discover,
            dns_resolve::DnsResolve,
            resolve::{map_endpoint, recover},
        },
        svc,
    };
    use linkerd_error::Recover;
    use std::net::SocketAddr;

    pub fn layer<M, R>(
        dns: dns::Resolver,
        recover: R,
    ) -> impl svc::Layer<M, Service = Discover<M, R>>
    where
        R: Recover + Clone,
        R::Backoff: Unpin,
    {
        svc::layer::mk(move |endpoint| {
            discover::resolve(
                endpoint,
                map_endpoint::Resolve::new(
                    IntoTarget(()),
                    recover::Resolve::new(recover.clone(), DnsResolve::new(dns.clone())),
                ),
            )
        })
    }

    type Discover<M, R> = discover::MakeEndpoint<
        discover::FromResolve<
            map_endpoint::Resolve<IntoTarget, recover::Resolve<R, DnsResolve>>,
            Target,
        >,
        M,
    >;

    #[derive(Copy, Clone, Debug)]
    pub struct IntoTarget(());

    impl map_endpoint::MapEndpoint<super::ControlAddr, ()> for IntoTarget {
        type Out = Target;

        fn map_endpoint(&self, control: &super::ControlAddr, addr: SocketAddr, _: ()) -> Self::Out {
            Target::new(addr, control.identity.clone())
        }
    }
}

mod balance {
    use crate::proxy::http;
    use std::time::Duration;

    const EWMA_DEFAULT_RTT: Duration = Duration::from_millis(30);
    const EWMA_DECAY: Duration = Duration::from_secs(10);

    pub fn layer<A, B>() -> http::balance::Layer<A, B> {
        http::balance::layer(EWMA_DEFAULT_RTT, EWMA_DECAY)
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
        addr: SocketAddr,
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

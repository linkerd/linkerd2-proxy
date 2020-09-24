use crate::{
    classify, config, control, dns,
    proxy::http,
    reconnect,
    svc::{self, NewService},
    transport::tls,
    Addr, ControlHttpMetrics, Error,
};
use std::fmt;

#[derive(Clone, Debug)]
pub struct Config {
    pub addr: ControlAddr,
    pub connect: config::ConnectConfig,
    pub buffer_capacity: usize,
}

#[derive(Clone, Debug)]
pub struct ControlAddr {
    pub addr: Addr,
    pub identity: tls::PeerIdentity,
}

impl Into<Addr> for ControlAddr {
    fn into(self) -> Addr {
        self.addr
    }
}

impl fmt::Display for ControlAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.addr, f)
    }
}

type BalanceBody =
    http::balance::PendingUntilFirstDataBody<tower::load::peak_ewma::Handle, http::Payload>;

type RspBody = linkerd2_http_metrics::requests::ResponseBody<BalanceBody, classify::Eos>;

pub type Client<B> = linkerd2_buffer::Buffer<http::Request<B>, http::Response<RspBody>>;

impl Config {
    pub fn build<B, I>(
        self,
        dns: dns::Resolver,
        metrics: ControlHttpMetrics,
        identity: tls::Conditional<I>,
    ) -> Client<B>
    where
        B: http::HttpBody + Send + 'static,
        B::Data: Send,
        B::Error: Into<Error> + Send + Sync,
        I: Clone + tls::client::HasConfig + Send + 'static,
    {
        let backoff = {
            let backoff = self.connect.backoff;
            move |_| Ok(backoff.stream())
        };
        svc::connect(self.connect.keepalive)
            .push(tls::ConnectLayer::new(identity))
            .push_timeout(self.connect.timeout)
            .push(self::client::layer())
            .push(reconnect::layer(backoff.clone()))
            .push(self::resolve::layer(dns, backoff))
            .push_on_response(self::control::balance::layer())
            .into_new_service()
            .push(metrics.into_layer::<classify::Response>())
            .push(self::add_origin::layer())
            .push_on_response(svc::layers().push_spawn_buffer(self.buffer_capacity))
            .new_service(self.addr)
    }
}

/// Sets the request's URI from `Config`.
mod add_origin {
    use super::ControlAddr;
    use linkerd2_stack::{layer, NewService, ResultService};
    use std::marker::PhantomData;
    use tower_request_modifier::{Builder, RequestModifier};

    pub fn layer<M, B>() -> impl layer::Layer<M, Service = NewAddOrigin<M, B>> + Clone {
        layer::mk(|inner| NewAddOrigin {
            inner,
            _marker: PhantomData,
        })
    }

    #[derive(Debug)]
    pub struct NewAddOrigin<M, B> {
        inner: M,
        _marker: PhantomData<fn(B)>,
    }

    // === impl NewAddOrigin ===

    impl<M: Clone, B> Clone for NewAddOrigin<M, B> {
        fn clone(&self) -> Self {
            Self {
                inner: self.inner.clone(),
                _marker: self._marker,
            }
        }
    }

    impl<M, B> NewService<ControlAddr> for NewAddOrigin<M, B>
    where
        M: NewService<ControlAddr>,
    {
        type Service = ResultService<RequestModifier<M::Service, B>, BuildError>;

        fn new_service(&mut self, target: ControlAddr) -> Self::Service {
            let authority = target.addr.to_http_authority();
            ResultService::from(
                Builder::new()
                    .set_origin(format!("http://{}", authority))
                    .build(self.inner.new_service(target))
                    .map_err(|_| BuildError(())),
            )
        }
    }

    // XXX the request_modifier build error does not implement Error...
    #[derive(Debug)]
    pub struct BuildError(());

    impl std::error::Error for BuildError {}
    impl std::fmt::Display for BuildError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "failed to build the add-origin request modifier")
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
    use linkerd2_error::Recover;
    use std::net::SocketAddr;

    pub fn layer<M, R>(
        dns: dns::Resolver,
        recover: R,
    ) -> impl svc::Layer<M, Service = Discover<M, R>>
    where
        R: Recover + Clone,
        R::Backoff: Unpin,
    {
        discover::resolve(map_endpoint::Resolve::new(
            IntoTarget(()),
            recover::Resolve::new(recover, DnsResolve::new(dns)),
        ))
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
    use crate::transport::tls;
    use crate::{proxy::http, svc};
    use linkerd2_proxy_http::h2::Settings as H2Settings;
    use std::{
        net::SocketAddr,
        task::{Context, Poll},
    };

    #[derive(Clone, Hash, Debug, Eq, PartialEq)]
    pub struct Target {
        addr: SocketAddr,
        server_name: tls::PeerIdentity,
    }

    impl Target {
        pub(super) fn new(addr: SocketAddr, server_name: tls::PeerIdentity) -> Self {
            Self { addr, server_name }
        }
    }

    #[derive(Debug)]
    pub struct Client<C, B> {
        inner: http::h2::Connect<C, B>,
    }

    // === impl Target ===

    impl Into<SocketAddr> for Target {
        fn into(self) -> SocketAddr {
            self.addr
        }
    }

    impl tls::HasPeerIdentity for Target {
        fn peer_identity(&self) -> tls::PeerIdentity {
            self.server_name.clone()
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

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

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

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
    http::balance::PendingUntilFirstDataBody<tower::load::peak_ewma::Handle, http::glue::Body>;

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
            .push_spawn_ready()
            .push(self::resolve::layer(dns, backoff))
            .push_on_response(self::control::balance::layer())
            .push(metrics.into_layer::<classify::Response>())
            .push(self::add_origin::Layer::new())
            .into_new_service()
            .check_new_service()
            .push_on_response(svc::layers().push_spawn_buffer(self.buffer_capacity))
            .new_service(self.addr)
    }
}

/// Sets the request's URI from `Config`.
mod add_origin {
    use super::ControlAddr;
    use futures::{ready, TryFuture};
    use linkerd2_error::Error;
    use pin_project::pin_project;
    use std::future::Future;
    use std::marker::PhantomData;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tower_request_modifier::{Builder, RequestModifier};

    #[derive(Debug)]
    pub struct Layer<B> {
        _marker: PhantomData<fn(B)>,
    }

    #[derive(Debug)]
    pub struct MakeAddOrigin<M, B> {
        inner: M,
        _marker: PhantomData<fn(B)>,
    }

    #[pin_project]
    pub struct MakeFuture<F, B> {
        #[pin]
        inner: F,
        authority: http::uri::Authority,
        _marker: PhantomData<fn(B)>,
    }

    // === impl Layer ===

    impl<B> Layer<B> {
        pub fn new() -> Self {
            Layer {
                _marker: PhantomData,
            }
        }
    }

    impl<B> Clone for Layer<B> {
        fn clone(&self) -> Self {
            Self {
                _marker: self._marker,
            }
        }
    }

    impl<M, B> tower::layer::Layer<M> for Layer<B> {
        type Service = MakeAddOrigin<M, B>;

        fn layer(&self, inner: M) -> Self::Service {
            Self::Service {
                inner,
                _marker: PhantomData,
            }
        }
    }

    // === impl MakeAddOrigin ===

    impl<M, B> tower::Service<ControlAddr> for MakeAddOrigin<M, B>
    where
        M: tower::Service<ControlAddr>,
        M::Error: Into<Error>,
    {
        type Response = RequestModifier<M::Response, B>;
        type Error = Error;
        type Future = MakeFuture<M::Future, B>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx).map_err(Into::into)
        }

        fn call(&mut self, target: ControlAddr) -> Self::Future {
            let authority = target.addr.to_http_authority();
            let inner = self.inner.call(target);
            MakeFuture {
                inner,
                authority,
                _marker: PhantomData,
            }
        }
    }

    impl<M, B> Clone for MakeAddOrigin<M, B>
    where
        M: tower::Service<ControlAddr> + Clone,
    {
        fn clone(&self) -> Self {
            Self {
                inner: self.inner.clone(),
                _marker: PhantomData,
            }
        }
    }

    // === impl MakeFuture ===

    impl<F, B> Future for MakeFuture<F, B>
    where
        F: TryFuture,
        F::Error: Into<Error>,
    {
        type Output = Result<RequestModifier<F::Ok, B>, Error>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.project();
            let inner = ready!(this.inner.try_poll(cx).map_err(Into::into))?;

            Poll::Ready(
                Builder::new()
                    .set_origin(format!("http://{}", this.authority))
                    .build(inner)
                    .map_err(|_| BuildError.into()),
            )
        }
    }

    // XXX the request_modifier build error does not implement Error...
    #[derive(Debug)]
    struct BuildError;

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
    use crate::transport::{connect, tls};
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

    impl connect::ConnectAddr for Target {
        fn connect_addr(&self) -> SocketAddr {
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

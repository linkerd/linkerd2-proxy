use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::Arc;

use futures::Poll;
use http::{self, uri};
use tower_service as tower;
use tower_h2;

use control::destination::Endpoint;
use ctx;
use svc::{MakeClient, Reconnect};
use telemetry;
use proxy;
use transport;
use tls;
use ctx::transport::TlsStatus;
use watch_service::{WatchService, Rebind};

/// An HTTP `Service` that is created for each `Endpoint` and `Protocol`.
pub type Stack<B> = proxy::http::orig_proto::Upgrade<NormalizeUri<WatchTls<B>>>;

type WatchTls<B> = WatchService<tls::ConditionalClientConfig, RebindTls<B>>;

/// An HTTP `Service` that is created for each `Endpoint`, `Protocol`, and client
/// TLS configuration.
pub type TlsStack<B> = telemetry::http::service::Http<HttpService<B>, B, proxy::http::Body>;

type HttpService<B> = Reconnect<
    Arc<ctx::transport::Client>,
    proxy::http::Client<
        transport::metrics::Connect<transport::Connect>,
        ::logging::ClientExecutor<&'static str, SocketAddr>,
        telemetry::http::service::RequestBody<B>,
    >
>;

/// Binds a `Service` from a `SocketAddr`.
///
/// The returned `Service` buffers request until a connection is established.
///
/// # TODO
///
/// Buffering is not bounded and no timeouts are applied.
pub struct Bind<C, B> {
    ctx: C,
    sensors: telemetry::Sensors,
    transport_registry: transport::metrics::Registry,
    tls_client_config: tls::ClientConfigWatch,
    _p: PhantomData<fn() -> B>,
}

/// Binds a `Service` from a `SocketAddr` for a pre-determined protocol.
pub struct BindProtocol<C, B> {
    bind: Bind<C, B>,
    protocol: Protocol,
}

/// A bound service that can re-bind itself on demand.
///
/// Reasons this would need to re-bind:
///
/// - `BindsPerRequest` can only service 1 request, and then needs to bind a
///   new service.
/// - If there is an error in the inner service (such as a connect error), we
///   need to throw it away and bind a new service.
pub struct BoundService<B>
where
    B: tower_h2::Body + Send + 'static,
    <B::Data as ::bytes::IntoBuf>::Buf: Send,
{
    bind: Bind<ctx::Proxy, B>,
    binding: Binding<B>,
    endpoint: Endpoint,
    protocol: Protocol,
}

/// A type of service binding.
///
/// Some services, for various reasons, may not be able to be used to serve multiple
/// requests. The `BindsPerRequest` binding ensures that a new stack is bound for each
/// request.
///
/// `Bound` services may be used to process an arbitrary number of requests.
pub enum Binding<B>
where
    B: tower_h2::Body + Send + 'static,
    <B::Data as ::bytes::IntoBuf>::Buf: Send,
{
    Bound(Stack<B>),
    BindsPerRequest {
        // When `poll_ready` is called, the _next_ service to be used may be bound
        // ahead-of-time. This stack is used only to serve the next request to this
        // service.
        next: Option<Stack<B>>
    },
}

/// Protocol portion of the `Recognize` key for a request.
///
/// This marks whether to use HTTP/2 or HTTP/1.x for a request. In
/// the case of HTTP/1.x requests, it also stores a "host" key to ensure
/// that each host receives its own connection.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    Http1 {
        host: Host,
        /// Whether the request wants to use HTTP/1.1's Upgrade mechanism.
        ///
        /// Since these cannot be translated into orig-proto, it must be
        /// tracked here so as to allow those with `is_h1_upgrade: true` to
        /// use an explicitly HTTP/1 service, instead of one that might
        /// utilize orig-proto.
        is_h1_upgrade: bool,
        /// Whether or not the request URI was in absolute form.
        ///
        /// This is used to configure Hyper's behaviour at the connection
        /// level, so it's necessary that requests with and without
        /// absolute URIs be bound to separate service stacks. It is also
        /// used to determine what URI normalization will be necessary.
        was_absolute_form: bool,
    },
    Http2
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Host {
    Authority(uri::Authority),
    NoAuthority,
}

/// Rewrites HTTP/1.x requests so that their URIs are in a canonical form.
///
/// The following transformations are applied:
/// - If an absolute-form URI is received, it must replace
///   the host header (in accordance with RFC7230#section-5.4)
/// - If the request URI is not in absolute form, it is rewritten to contain
///   the authority given in the `Host:` header, or, failing that, from the
///   request's original destination according to `SO_ORIGINAL_DST`.
#[derive(Copy, Clone, Debug)]
pub struct NormalizeUri<S> {
    inner: S,
    was_absolute_form: bool,
}

pub struct RebindTls<B> {
    bind: Bind<ctx::Proxy, B>,
    protocol: Protocol,
    endpoint: Endpoint,
}

#[derive(Copy, Clone, Debug)]
pub enum BufferSpawnError {
    Inbound,
    Outbound,
}

impl fmt::Display for BufferSpawnError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.pad(self.description())
    }
}

impl Error for BufferSpawnError {

    fn description(&self) -> &str {
        match *self {
            BufferSpawnError::Inbound =>
                "error spawning inbound buffer task",
            BufferSpawnError::Outbound =>
                "error spawning outbound buffer task",
        }
    }

    fn cause(&self) -> Option<&Error> { None }
}

impl<B> Bind<(), B> {
    pub fn new(
        sensors: telemetry::Sensors,
        transport_registry: transport::metrics::Registry,
        tls_client_config: tls::ClientConfigWatch
    ) -> Self {
        Self {
            ctx: (),
            sensors,
            transport_registry,
            tls_client_config,
            _p: PhantomData,
        }
    }

    pub fn with_ctx<C>(self, ctx: C) -> Bind<C, B> {
        Bind {
            ctx,
            sensors: self.sensors,
            transport_registry: self.transport_registry,
            tls_client_config: self.tls_client_config,
            _p: PhantomData,
        }
    }
}

impl<C: Clone, B> Clone for Bind<C, B> {
    fn clone(&self) -> Self {
        Self {
            ctx: self.ctx.clone(),
            sensors: self.sensors.clone(),
            transport_registry: self.transport_registry.clone(),
            tls_client_config: self.tls_client_config.clone(),
            _p: PhantomData,
        }
    }
}

impl<B> Bind<ctx::Proxy, B>
where
    B: tower_h2::Body + Send + 'static,
    <B::Data as ::bytes::IntoBuf>::Buf: Send,
{
    /// Binds the innermost layers of the stack with a TLS configuration.
    ///
    /// A reconnecting HTTP client is established with the given endpont,
    /// protocol, and TLS configuration.
    ///
    /// This client is instrumented with metrics.
    fn bind_with_tls(
        &self,
        ep: &Endpoint,
        protocol: &Protocol,
        tls_client_config: &tls::ConditionalClientConfig,
    ) -> TlsStack<B> {
        debug!("bind_with_tls endpoint={:?}, protocol={:?}", ep, protocol);
        let addr = ep.address();

        let log = ::logging::Client::proxy(self.ctx, addr)
            .with_protocol(protocol.clone());

        let tls = ep.tls_identity().and_then(|identity| {
            tls_client_config.as_ref().map(|config| {
                tls::ConnectionConfig {
                    server_identity: identity.clone(),
                    config: config.clone(),
                }
            })
        });

        let client_ctx = ctx::transport::Client::new(
            self.ctx,
            &addr,
            ep.metadata().clone(),
            TlsStatus::from(&tls),
        );

        // Map a socket address to a connection.
        let connect = self.transport_registry
            .new_connect(client_ctx.as_ref(), transport::Connect::new(addr, tls));

        // TODO: Add some sort of backoff logic between reconnects.
        self.sensors.http(
            client_ctx.clone(),
            Reconnect::new(
                client_ctx.clone(),
                proxy::http::Client::new(protocol, connect, log.executor())
            )
        )
   }

    /// Build a `Service` for the given endpoint and `Protocol`.
    ///
    /// The service attempts to upgrade HTTP/1 requests to HTTP/2 (if it's known
    /// with prior knowledge that the endpoint supports HTTP/2).
    ///
    /// As `tls_client_config` updates, `bind_with_tls` is called to rebuild the
    /// client with the appropriate TLS configuraiton.
    fn bind_stack(&self, ep: &Endpoint, protocol: &Protocol) -> Stack<B> {
        debug!("bind_stack: endpoint={:?}, protocol={:?}", ep, protocol);
        let rebind = RebindTls {
            bind: self.clone(),
            endpoint: ep.clone(),
            protocol: protocol.clone(),
        };
        let watch_tls = WatchService::new(self.tls_client_config.clone(), rebind);

        // HTTP/1.1 middlewares
        //
        // TODO make this conditional based on `protocol`
        // TODO extract HTTP/1 rebinding logic up here

        // Rewrite the HTTP/1 URI, if the authorities in the Host header
        // and request URI are not in agreement, or are not present.
        //
        // TODO move this into proxy::Client?
        let normalize_uri = NormalizeUri::new(watch_tls, protocol.was_absolute_form());

        // Upgrade HTTP/1.1 requests to be HTTP/2 if the endpoint supports HTTP/2.
        proxy::http::orig_proto::Upgrade::new(normalize_uri, protocol.is_http2())
    }

    pub fn bind_service(&self, ep: &Endpoint, protocol: &Protocol) -> BoundService<B> {
        // If the endpoint is another instance of this proxy, AND the usage
        // of HTTP/1.1 Upgrades are not needed, then bind to an HTTP2 service
        // instead.
        //
        // The related `orig_proto` middleware will automatically translate
        // if the protocol was originally HTTP/1.
        let protocol = if ep.can_use_orig_proto() && !protocol.is_h1_upgrade() {
            &Protocol::Http2
        } else {
            protocol
        };

        let binding = if protocol.can_reuse_clients() {
            Binding::Bound(self.bind_stack(ep, protocol))
        } else {
            Binding::BindsPerRequest {
                next: None
            }
        };

        BoundService {
            bind: self.clone(),
            binding,
            endpoint: ep.clone(),
            protocol: protocol.clone(),
        }
    }
}

// ===== impl BindProtocol =====


impl<C, B> Bind<C, B> {
    pub fn with_protocol(self, protocol: Protocol)
        -> BindProtocol<C, B>
    {
        BindProtocol {
            bind: self,
            protocol,
        }
    }
}

impl<B> MakeClient<Endpoint> for BindProtocol<ctx::Proxy, B>
where
    B: tower_h2::Body + Send + 'static,
    <B::Data as ::bytes::IntoBuf>::Buf: Send,
{
    type Error = ();
    type Client = BoundService<B>;

    fn make_client(&self, ep: &Endpoint) -> Result<Self::Client, ()> {
        Ok(self.bind.bind_service(ep, &self.protocol))
    }
}


// ===== impl NormalizeUri =====

impl<S> NormalizeUri<S> {
    fn new(inner: S, was_absolute_form: bool) -> Self {
        Self { inner, was_absolute_form }
    }
}

impl<S, B> tower::Service for NormalizeUri<S>
where
    S: tower::Service<Request = http::Request<B>>,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), S::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut request: S::Request) -> Self::Future {
        if request.version() != http::Version::HTTP_2 &&
            // Skip normalizing the URI if it was received in
            // absolute form.
            !self.was_absolute_form
        {
            proxy::http::h1::normalize_our_view_of_uri(&mut request);
        }
        self.inner.call(request)
    }
}
// ===== impl Binding =====

impl<B> tower::Service for BoundService<B>
where
    B: tower_h2::Body + Send + 'static,
    <B::Data as ::bytes::IntoBuf>::Buf: Send,
{
    type Request = <Stack<B> as tower::Service>::Request;
    type Response = <Stack<B> as tower::Service>::Response;
    type Error = <Stack<B> as tower::Service>::Error;
    type Future = <Stack<B> as tower::Service>::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match self.binding {
            // A service is already bound, so poll its readiness.
            Binding::Bound(ref mut svc) |
            Binding::BindsPerRequest { next: Some(ref mut svc) } => {
                trace!("poll_ready: stack already bound");
                svc.poll_ready()
            }

            // If no stack has been bound, bind it now so that its readiness can be
            // checked. Store it so it can be consumed to dispatch the next request.
            Binding::BindsPerRequest { ref mut next } => {
                trace!("poll_ready: binding stack");
                let mut svc = self.bind.bind_stack(&self.endpoint, &self.protocol);
                let ready = svc.poll_ready();
                *next = Some(svc);
                ready
            }
        }
    }

    fn call(&mut self, request: Self::Request) -> Self::Future {
        match self.binding {
            Binding::Bound(ref mut svc) => svc.call(request),
            Binding::BindsPerRequest { ref mut next } => {
                let mut svc = next.take().expect("poll_ready must be called before call");
                svc.call(request)
            }
        }
    }
}

// ===== impl Protocol =====


impl Protocol {
    pub fn detect<B>(req: &http::Request<B>) -> Self {
        if req.version() == http::Version::HTTP_2 {
            return Protocol::Http2;
        }

        let was_absolute_form = proxy::http::h1::is_absolute_form(req.uri());
        trace!(
            "Protocol::detect(); req.uri='{:?}'; was_absolute_form={:?};",
            req.uri(), was_absolute_form
        );
        // If the request has an authority part, use that as the host part of
        // the key for an HTTP/1.x request.
        let host = Host::detect(req);

        let is_h1_upgrade = proxy::http::h1::wants_upgrade(req);

        Protocol::Http1 {
            host,
            is_h1_upgrade,
            was_absolute_form,
        }
    }

    /// Returns true if the request was originally received in absolute form.
    pub fn was_absolute_form(&self) -> bool {
        match self {
            &Protocol::Http1 { was_absolute_form, .. } => was_absolute_form,
            _ => false,
        }
    }

    pub fn can_reuse_clients(&self) -> bool {
        match *self {
            Protocol::Http2 | Protocol::Http1 { host: Host::Authority(_), .. } => true,
            _ => false,
        }
    }

    pub fn is_h1_upgrade(&self) -> bool {
        match *self {
            Protocol::Http1 { is_h1_upgrade: true, .. } => true,
            _ => false,
        }
    }

    pub fn is_http2(&self) -> bool {
        match *self {
            Protocol::Http2 => true,
            _ => false,
        }
    }
}

impl Host {
    pub fn detect<B>(req: &http::Request<B>) -> Host {
        req
            .uri()
            .authority_part()
            .cloned()
            .or_else(|| proxy::http::h1::authority_from_host(req))
            .map(Host::Authority)
            .unwrap_or_else(|| Host::NoAuthority)
    }
}

// ===== impl RebindTls =====

impl<B> Rebind<tls::ConditionalClientConfig> for RebindTls<B>
where
    B: tower_h2::Body + Send + 'static,
    <B::Data as ::bytes::IntoBuf>::Buf: Send,
{
    type Service = TlsStack<B>;
    fn rebind(&mut self, tls: &tls::ConditionalClientConfig) -> Self::Service {
        debug!(
            "rebinding endpoint stack for {:?}:{:?} on TLS config change",
            self.endpoint, self.protocol,
        );
        self.bind.bind_with_tls(&self.endpoint, &self.protocol, tls)
    }
}

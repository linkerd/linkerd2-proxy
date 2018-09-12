use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::Arc;

use futures::Poll;
use tower_h2;

use control::destination::Endpoint;
use ctx;
use telemetry;
use proxy;
use proxy::http::Dialect;
use svc;
use transport;
use tls;
use ctx::transport::TlsStatus;
use watch_service::{WatchService, Rebind};

/// An HTTP `Service` that is created for each `Endpoint` and `Protocol`.
pub type Stack<B> = proxy::http::orig_proto::Upgrade<
    proxy::http::NormalizeUri<WatchTls<B>>
>;

type WatchTls<B> = WatchService<tls::ConditionalClientConfig, RebindTls<B>>;

/// An HTTP `Service` that is created for each `Endpoint`, `Dialect`, and client
/// TLS configuration.
pub type TlsStack<B> = telemetry::http::service::Http<HttpService<B>, B, proxy::http::Body>;

type HttpService<B> = svc::Reconnect<
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

/// Binds a `Service` from a `SocketAddr` for a pre-determined dialect.
pub struct BindDialect<C, B> {
    bind: Bind<C, B>,
    dialect: Dialect,
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
    dialect: Dialect,
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

pub struct RebindTls<B> {
    bind: Bind<ctx::Proxy, B>,
    dialect: Dialect,
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
    /// dialect, and TLS configuration.
    ///
    /// This client is instrumented with metrics.
    fn bind_with_tls(
        &self,
        ep: &Endpoint,
        dialect: &Dialect,
        tls_client_config: &tls::ConditionalClientConfig,
    ) -> TlsStack<B> {
        debug!("bind_with_tls endpoint={:?}, dialect={:?}", ep, dialect);
        let addr = ep.address();

        let log = ::logging::Client::proxy(self.ctx, addr)
            .with_dialect(dialect.clone());

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
            svc::Reconnect::new(
                client_ctx.clone(),
                proxy::http::Client::new(dialect, connect, log.executor())
            )
        )
   }

    /// Build a `Service` for the given endpoint and `Dialect`.
    ///
    /// The service attempts to upgrade HTTP/1 requests to HTTP/2 (if it's known
    /// with prior knowledge that the endpoint supports HTTP/2).
    ///
    /// As `tls_client_config` updates, `bind_with_tls` is called to rebuild the
    /// client with the appropriate TLS configuraiton.
    fn bind_stack(&self, ep: &Endpoint, dialect: &Dialect) -> Stack<B> {
        debug!("bind_stack: endpoint={:?}, dialect={:?}", ep, dialect);
        let rebind = RebindTls {
            bind: self.clone(),
            endpoint: ep.clone(),
            dialect: dialect.clone(),
        };
        let watch_tls = WatchService::new(self.tls_client_config.clone(), rebind);

        // HTTP/1.1 middlewares
        //
        // TODO make this conditional based on `dialect`
        // TODO extract HTTP/1 rebinding logic up here

        // Rewrite the HTTP/1 URI, if the authorities in the Host header
        // and request URI are not in agreement, or are not present.
        let normalize_uri = proxy::http::NormalizeUri::new(watch_tls, dialect.was_absolute_form());

        // Upgrade HTTP/1.1 requests to be HTTP/2 if the endpoint supports HTTP/2.
        proxy::http::orig_proto::Upgrade::new(normalize_uri, dialect.is_http2())
    }

    pub fn bind_service(&self, ep: &Endpoint, dialect: &Dialect) -> BoundService<B> {
        // If the endpoint is another instance of this proxy, AND the usage
        // of HTTP/1.1 Upgrades are not needed, then bind to an HTTP2 service
        // instead.
        //
        // The related `orig_proto` middleware will automatically translate
        // if the dialect was originally HTTP/1.
        let dialect = if ep.can_use_orig_proto() && !dialect.is_h1_upgrade() {
            &Dialect::Http2
        } else {
            dialect
        };

        let binding = if dialect.can_reuse_clients() {
            Binding::Bound(self.bind_stack(ep, dialect))
        } else {
            Binding::BindsPerRequest {
                next: None
            }
        };

        BoundService {
            bind: self.clone(),
            binding,
            endpoint: ep.clone(),
            dialect: dialect.clone(),
        }
    }
}

// ===== impl BindDialect =====


impl<C, B> Bind<C, B> {
    pub fn with_dialect(self, dialect: Dialect)
        -> BindDialect<C, B>
    {
        BindDialect {
            bind: self,
            dialect,
        }
    }
}

impl<B> svc::MakeClient<Endpoint> for BindDialect<ctx::Proxy, B>
where
    B: tower_h2::Body + Send + 'static,
    <B::Data as ::bytes::IntoBuf>::Buf: Send,
{
    type Error = ();
    type Client = BoundService<B>;

    fn make_client(&self, ep: &Endpoint) -> Result<Self::Client, ()> {
        Ok(self.bind.bind_service(ep, &self.dialect))
    }
}

// ===== impl Binding =====

impl<B> svc::Service for BoundService<B>
where
    B: tower_h2::Body + Send + 'static,
    <B::Data as ::bytes::IntoBuf>::Buf: Send,
{
    type Request = <Stack<B> as svc::Service>::Request;
    type Response = <Stack<B> as svc::Service>::Response;
    type Error = <Stack<B> as svc::Service>::Error;
    type Future = <Stack<B> as svc::Service>::Future;

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
                let mut svc = self.bind.bind_stack(&self.endpoint, &self.dialect);
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
            self.endpoint, self.dialect,
        );
        self.bind.bind_with_tls(&self.endpoint, &self.dialect, tls)
    }
}

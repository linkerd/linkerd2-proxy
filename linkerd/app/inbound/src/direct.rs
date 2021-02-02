use crate::{http, target::TcpEndpoint};
use linkerd_app_core::{
    config::ProxyConfig,
    detect, drain, io, metrics,
    opencensus::proto::trace::v1 as oc,
    proxy::identity::LocalCrtKey,
    svc::{self, stack::Param},
    tls,
    transport::{self, listen, metrics::SensorIo},
    transport_header::{self, DetectHeader, SessionProtocol, TransportHeader},
    Conditional, Error, NameAddr,
};
use std::{
    convert::{TryFrom, TryInto},
    fmt::Debug,
    net::SocketAddr,
};
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
struct WithTransportHeaderAlpn(LocalCrtKey);

type FwdIo<I> = io::PrefixedIo<SensorIo<tls::server::Io<I>>>;

/// Creates I/O errors when a connection cannot be forwarded because no transport
/// header was present.
#[derive(Debug, Default)]
struct RefusedNoHeader;

#[derive(Debug)]
struct RefusedNoIdentity;

#[derive(Debug)]
struct RefusedNoTarget;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HttpGatewayTarget {
    /// HTTP gateway targets *must* have a logical name.
    pub target: NameAddr,
    /// The outbound protocol, i.e. before upgrading.
    pub version: http::Version,
}

/// Routes HTTP requests to an `HttpGatewayTarget`.
#[derive(Clone, Debug)]
struct RouteHttpGatewayTarget(HttpClientInfo);

#[derive(Clone, Debug)]
struct HttpClientInfo {
    /// HTTP connections should have a target addr from the transport header.
    /// Legacy clients may not provide one, though.
    target: Option<NameAddr>,
    /// The serverside protocol, i.e. before downgrading.
    version: http::Version,
    client: ClientInfo,
}

/// Client connections *must* have an identity.
#[derive(Clone, Debug)]
struct ClientInfo {
    client_id: tls::ClientId,
    client_addr: SocketAddr,
    local_addr: SocketAddr,
}

/// Builds a stack that handles connections that target the proxy's inbound port
/// (i.e. without an SO_ORIGINAL_DST setting). This port behaves differently from
/// the main proxy stack:
///
/// 1. Protocol detection is always performed;
/// 2. TLS is required;
/// 3. A transport header is expected. It's not strictly required, as
///    gateways may need to accept HTTP requests from older proxy versions
pub fn stack<I, F, FSvc, H, HSvc>(
    config: &ProxyConfig,
    local_identity: Option<LocalCrtKey>,
    tcp: F,
    http: H,
    metrics: &metrics::Proxy,
    span_sink: Option<mpsc::Sender<oc::Span>>,
    drain: drain::Watch,
) -> impl svc::NewService<
    listen::Addrs,
    Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
> + Clone
where
    I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
    I: Debug + Send + Sync + Unpin + 'static,
    F: svc::NewService<TcpEndpoint, Service = FSvc> + Clone + Send + Sync + Unpin + 'static,
    FSvc: svc::Service<FwdIo<I>, Response = ()> + Clone + Send + Sync + Unpin + 'static,
    FSvc::Error: Into<Error>,
    FSvc::Future: Send + Unpin,
    H: svc::NewService<HttpGatewayTarget, Service = HSvc> + Clone + Send + Sync + Unpin + 'static,
    HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
        + Send
        + 'static,
    HSvc::Error: Into<Error>,
    HSvc::Future: Send,
{
    let detect_timeout = config.detect_protocol_timeout;
    let dispatch_timeout = config.dispatch_timeout;

    let http_server = {
        // Cache an HTTP gateway service for each destination and HTTP version.
        //
        // The destination is determined from the transport header or, if none was
        // present, the request URI.
        //
        // The client's ID is set as a request extension, as required by the
        // gateway. This permits gateway services (and profile resolutions) to be
        // cached per target, shared across clients.
        let gateway = svc::stack(http)
            .push_on_response(
                svc::layers()
                    .push(svc::layer::mk(svc::SpawnReady::new))
                    .push(metrics.stack.layer(crate::stack_labels("http", "gateway")))
                    .push(svc::FailFast::layer("Gateway", dispatch_timeout))
                    .push_spawn_buffer(config.buffer_capacity),
            )
            .push_cache(config.cache_max_idle_age)
            .push_on_response(
                svc::layers()
                    .push(http::Retain::layer())
                    .push(http::BoxResponse::layer()),
            )
            .check_new_service::<HttpGatewayTarget, http::Request<_>>()
            .push(svc::NewRouter::layer(RouteHttpGatewayTarget))
            .check_new_service::<HttpClientInfo, http::Request<_>>()
            .push_http_insert_target::<tls::ClientId>()
            .into_inner();

        http::server(&config, gateway, metrics, span_sink, drain)
    };

    svc::stack(tcp)
        .check_new_service::<TcpEndpoint, FwdIo<I>>()
        // When the transport header is present, it may be used for either local
        // TCP forwarding, or we may be processing an HTTP gateway connection.
        // HTTP gateway connections that have a transport header must provide a
        // target name as a part of the header.
        .push_switch(
            |(h, client): (TransportHeader, ClientInfo)| match h {
                TransportHeader {
                    port,
                    protocol: None,
                    ..
                } => Ok(svc::Either::A(TcpEndpoint { port })),
                TransportHeader {
                    port,
                    name: Some(name),
                    protocol: Some(proto),
                } => Ok(svc::Either::B(HttpClientInfo {
                    client,
                    target: Some(NameAddr::from((name, port))),
                    version: match proto {
                        SessionProtocol::Http1 => http::Version::Http1,
                        SessionProtocol::Http2 => http::Version::H2,
                    },
                })),
                TransportHeader {
                    name: None,
                    protocol: Some(_),
                    ..
                } => Err(RefusedNoTarget),
            },
            // HTTP detection is not necessary in this case, since the transport
            // header indicates the connection's HTTP version.
            svc::stack(http_server.clone())
                .push_on_response(svc::layers().push_map_target(io::EitherIo::Left))
                .check_new_service::<HttpClientInfo, FwdIo<I>>()
                .into_inner(),
        )
        // When the transport header is not present, perform HTTP detection to
        // support legacy gateway clients.
        //
        // TODO: Stop supporting headerless connections after we have at least
        // one stable release out with transport header support.
        .push(svc::UnwrapOr::layer(
            svc::stack(http_server)
                .check_new_service::<HttpClientInfo, _>()
                .push_on_response(svc::layers().push_map_target(io::EitherIo::Right))
                .push_map_target(
                    |(version, client): (http::Version, ClientInfo)| HttpClientInfo {
                        version,
                        client,
                        target: None,
                    },
                )
                .push(svc::UnwrapOr::layer(
                    svc::Fail::<_, RefusedNoHeader>::default(),
                ))
                .push(detect::NewDetectService::timeout(
                    detect_timeout,
                    http::DetectHttp::default(),
                ))
                .check_new_service::<ClientInfo, FwdIo<I>>()
                .into_inner(),
        ))
        // We always try to detect a protocol header. While we can know whether
        // it's expected based on the serverside ALPN (passed via the target),
        // it's easier to just do detection and handle the case when it's not
        // present as an exception.
        .push(detect::NewDetectService::timeout(
            detect_timeout,
            DetectHeader::default(),
        ))
        .push(metrics.transport.layer_accept())
        // Build a ClientInfo target for each accepted connection. Refuse the
        // connection if it doesn't include an mTLS identity.
        .push_request_filter(ClientInfo::try_from)
        .push(tls::NewDetectTls::layer(
            local_identity.map(WithTransportHeaderAlpn),
            config.detect_protocol_timeout,
        ))
        .check_new_service::<listen::Addrs, I>()
        .into_inner()
}

// === impl ClientInfo ===

impl TryFrom<(tls::ConditionalServerTls, listen::Addrs)> for ClientInfo {
    type Error = RefusedNoIdentity;

    fn try_from(
        (tls, addrs): (tls::ConditionalServerTls, listen::Addrs),
    ) -> Result<Self, Self::Error> {
        match tls {
            Conditional::Some(tls::ServerTls::Established {
                client_id: Some(client_id),
                ..
            }) => Ok(ClientInfo {
                client_id,
                client_addr: addrs.peer(),
                local_addr: addrs.target_addr(),
            }),
            _ => Err(RefusedNoIdentity),
        }
    }
}

impl Param<transport::labels::Key> for ClientInfo {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::accept(
            transport::labels::Direction::In,
            Conditional::Some(tls::ServerTls::Established {
                client_id: Some(self.client_id.clone()),
                negotiated_protocol: None, // unused
            }),
            self.local_addr,
        )
    }
}

// === impl HttpClientInfo ===

impl Param<http::normalize_uri::DefaultAuthority> for HttpClientInfo {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        http::normalize_uri::DefaultAuthority(self.target.as_ref().map(NameAddr::as_http_authority))
    }
}

impl Param<http::Version> for HttpClientInfo {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl Param<tls::ClientId> for HttpClientInfo {
    fn param(&self) -> tls::ClientId {
        self.client.client_id.clone()
    }
}

// === impl RouteHttpGatewayTarget ===

impl<B> svc::stack::RecognizeRoute<http::Request<B>> for RouteHttpGatewayTarget {
    type Key = HttpGatewayTarget;

    fn recognize(&self, req: &http::Request<B>) -> Result<Self::Key, Error> {
        let version = req.version().try_into()?;

        if let Some(target) = self.0.target.clone() {
            return Ok(HttpGatewayTarget { target, version });
        }

        if let Some(a) = req.uri().authority() {
            let target = NameAddr::from_authority_with_default_port(a, 80)?;
            return Ok(HttpGatewayTarget { target, version });
        }

        Err(RefusedNoTarget.into())
    }
}

// === impl WithTransportHeaderAlpn ===

impl svc::stack::Param<tls::server::Config> for WithTransportHeaderAlpn {
    fn param(&self) -> tls::server::Config {
        // Copy the underlying TLS config and set an ALPN value.
        //
        // TODO: Avoid cloning the server config for every connection. It would
        // be preferable if rustls::ServerConfig wrapped individual fields in an
        // Arc so they could be overridden independently.
        let mut config = self.0.server_config().as_ref().clone();
        config
            .alpn_protocols
            .push(transport_header::PROTOCOL.into());
        config.into()
    }
}

impl svc::stack::Param<tls::LocalId> for WithTransportHeaderAlpn {
    fn param(&self) -> tls::LocalId {
        self.0.id().clone()
    }
}

// === impl RefusedNoHeader ===

impl Into<Error> for RefusedNoHeader {
    fn into(self) -> Error {
        Error::from(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "Non-transport-header connection refused",
        ))
    }
}

// === impl RefusedNoIdentity ===

impl std::fmt::Display for RefusedNoIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Direct connections must be mutually authenticated")
    }
}

impl std::error::Error for RefusedNoIdentity {}

// === impl RefusedNoTarget ===

impl std::fmt::Display for RefusedNoTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "A named target must be provided on gateway connections")
    }
}

impl std::error::Error for RefusedNoTarget {}

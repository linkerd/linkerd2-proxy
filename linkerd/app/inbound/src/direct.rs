use crate::{
    policy::{self, AllowPolicy},
    Inbound,
};
use linkerd_app_core::{
    identity, io,
    proxy::http,
    svc::{self, ExtractParam, InsertParam, Param},
    tls,
    transport::{self, metrics::SensorIo, ClientAddr, OrigDstAddr, Remote, ServerAddr},
    transport_header::{self, NewTransportHeaderServer, SessionProtocol, TransportHeader},
    Conditional, Error, Infallible, NameAddr, Result,
};
use std::{convert::TryFrom, fmt::Debug};
use thiserror::Error;
use tracing::{debug_span, info_span};

/// Creates I/O errors when a connection cannot be forwarded because no transport
/// header was present.
#[derive(Debug, Default)]
struct RefusedNoHeader;

#[derive(Debug, Error)]
#[error("direct connections must be mutually authenticated")]
pub struct RefusedNoIdentity(());

#[derive(Debug, Error)]
#[error("a named target must be provided on gateway connections")]
struct RefusedNoTarget;

#[derive(Debug, Clone)]
pub(crate) struct LocalTcp {
    client_addr: Remote<ClientAddr>,
    server_addr: Remote<ServerAddr>,
    client_id: tls::ClientId,
    policy: policy::AllowPolicy,
}

#[derive(Debug, Clone)]
pub(crate) struct AuthorizedLocalTcp {
    addr: Remote<ServerAddr>,
    client_id: tls::ClientId,
    permit: policy::ServerPermit,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalHttp {
    addr: Remote<ServerAddr>,
    policy: policy::AllowPolicy,
    client: ClientInfo,
    protocol: SessionProtocol,
}

type Local = svc::Either<LocalTcp, LocalHttp>;

#[derive(Debug, Clone)]
pub struct GatewayTransportHeader {
    pub target: NameAddr,
    pub protocol: Option<SessionProtocol>,
    pub client: ClientInfo,
    pub policy: policy::AllowPolicy,
}

/// Client connections *must* have an identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    pub client_id: tls::ClientId,
    pub alpn: Option<tls::NegotiatedProtocol>,
    pub client_addr: Remote<ClientAddr>,
    pub local_addr: OrigDstAddr,
}

type TlsIo<I> = tls::server::Io<identity::ServerIo<tls::server::DetectIo<I>>, I>;
type FwdIo<I> = SensorIo<io::PrefixedIo<TlsIo<I>>>;
pub type GatewayIo<I> = FwdIo<I>;

#[derive(Clone)]
struct TlsParams {
    timeout: tls::server::Timeout,
    identity: identity::Server,
}

impl<N> Inbound<N> {
    /// Builds a stack that handles connections that target the proxy's inbound port
    /// (i.e. without an SO_ORIGINAL_DST setting). This port behaves differently from
    /// the main proxy stack:
    ///
    /// 1. Protocol detection is always performed;
    /// 2. TLS is required;
    /// 3. A transport header is expected. It's not strictly required, as
    ///    gateways may need to accept HTTP requests from older proxy versions
    pub(crate) fn push_direct<T, I, NSvc, G, GSvc, H, HSvc>(
        self,
        policies: impl policy::GetPolicy + Clone + Send + Sync + 'static,
        gateway: G,
        http: H,
    ) -> Inbound<svc::ArcNewTcp<T, I>>
    where
        T: Param<Remote<ClientAddr>> + Param<OrigDstAddr>,
        T: Clone + Send + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        N: svc::NewService<AuthorizedLocalTcp, Service = NSvc>,
        N: Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<FwdIo<I>, Response = ()> + Clone + Send + Sync + Unpin + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send + Unpin,
        G: svc::NewService<GatewayTransportHeader, Service = GSvc>,
        G: Clone + Send + Sync + Unpin + 'static,
        GSvc: svc::Service<GatewayIo<I>, Response = ()> + Send + 'static,
        GSvc::Error: Into<Error>,
        GSvc::Future: Send,
        H: svc::NewService<LocalHttp, Service = HSvc> + Clone + Send + Sync + Unpin + 'static,
        HSvc: svc::Service<io::PrefixedIo<TlsIo<I>>, Response = ()> + Send + 'static,
        HSvc::Error: Into<Error>,
        HSvc::Future: Send,
    {
        self.map_stack(|config, rt, inner| {
            let detect_timeout = config.proxy.detect_protocol_timeout;

            let identity = rt
                .identity
                .server()
                .with_alpn(vec![transport_header::PROTOCOL.into()])
                .expect("TLS credential store must be held");

            inner
                .push(transport::metrics::NewServer::layer(
                    rt.metrics.proxy.transport.clone(),
                ))
                .check_new_service::<AuthorizedLocalTcp, _>()
                .push_map_target(|(permit, tcp): (policy::ServerPermit, LocalTcp)| {
                    AuthorizedLocalTcp {
                        addr: tcp.server_addr,
                        client_id: tcp.client_id,
                        permit,
                    }
                })
                .check_new_service::<(policy::ServerPermit, LocalTcp), _>()
                .push(policy::NewTcpPolicy::layer(rt.metrics.tcp_authz.clone()))
                .instrument(|_: &_| debug_span!("opaque"))
                .check_new_service::<LocalTcp, _>()
                // When the transport header is present, it may be used for either local TCP
                // forwarding, or we may be processing an HTTP gateway connection. HTTP gateway
                // connections that have a transport header must provide a target name as a part of
                // the header.
                .push_switch(Ok::<Local, Infallible>, http)
                .push_switch(
                    {
                        let policies = policies.clone();
                        move |(h, client): (TransportHeader, ClientInfo)| -> Result<_> {
                            match h {
                                TransportHeader {
                                    port,
                                    name: None,
                                    protocol,
                                } => {
                                    // When the transport header targets an alternate port (but does
                                    // not identify an alternate target name), we check the new
                                    // target's policy to determine whether the client can access
                                    // it.
                                    let addr = (client.local_addr.ip(), port).into();
                                    let policy = policies.get_policy(OrigDstAddr(addr));
                                    let local = match protocol {
                                        None => svc::Either::A(LocalTcp {
                                            server_addr: Remote(ServerAddr(addr)),
                                            client_addr: client.client_addr,
                                            client_id: client.client_id,
                                            policy,
                                        }),
                                        Some(protocol) => {
                                            // When TransportHeader includes the protocol, but does not
                                            // include an alternate name we go through the Inbound HTTP
                                            // stack.
                                            svc::Either::B(LocalHttp {
                                                addr: Remote(ServerAddr(addr)),
                                                policy,
                                                protocol,
                                                client,
                                            })
                                        }
                                    };
                                    Ok(svc::Either::A(local))
                                }
                                TransportHeader {
                                    port,
                                    name: Some(name),
                                    protocol,
                                } => {
                                    // When the transport header provides an alternate target, the
                                    // connection is a gateway connection. We check the _gateway
                                    // address's_ policy to determine whether the client is
                                    // authorized to use this gateway.
                                    let policy = policies.get_policy(client.local_addr);
                                    Ok(svc::Either::B(GatewayTransportHeader {
                                        target: NameAddr::from((name, port)),
                                        protocol,
                                        client,
                                        policy,
                                    }))
                                }
                            }
                        }
                    },
                    // HTTP detection is not necessary in this case, since the transport
                    // header indicates the connection's HTTP version.
                    svc::stack(gateway.clone())
                        .push(transport::metrics::NewServer::layer(
                            rt.metrics.proxy.transport.clone(),
                        ))
                        .instrument(
                            |g: &GatewayTransportHeader| info_span!("gateway", dst = %g.target),
                        )
                        .into_inner(),
                )
                .check_new_service::<(TransportHeader, ClientInfo), _>()
                // Use ALPN to determine whether a transport header should be read.
                .push(NewTransportHeaderServer::layer(detect_timeout))
                .check_new_service::<ClientInfo, _>()
                .push_request_filter(|client: ClientInfo| -> Result<_> {
                    if client.header_negotiated() {
                        Ok(client)
                    } else {
                        Err(RefusedNoTarget.into())
                    }
                })
                // Build a ClientInfo target for each accepted connection. Refuse the
                // connection if it doesn't include an mTLS identity.
                .push_request_filter(ClientInfo::try_from)
                .push(svc::ArcNewService::layer())
                .push(tls::NewDetectTls::<identity::Server, _, _>::layer(
                    TlsParams {
                        timeout: tls::server::Timeout(detect_timeout),
                        identity,
                    },
                ))
                .check_new_service::<T, I>()
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl ClientInfo ===

impl<T> TryFrom<(tls::ConditionalServerTls, T)> for ClientInfo
where
    T: Param<OrigDstAddr>,
    T: Param<Remote<ClientAddr>>,
{
    type Error = Error;

    fn try_from((tls, addrs): (tls::ConditionalServerTls, T)) -> Result<Self, Self::Error> {
        match tls {
            Conditional::Some(tls::ServerTls::Established {
                client_id: Some(client_id),
                negotiated_protocol,
            }) => Ok(Self {
                client_id,
                alpn: negotiated_protocol,
                client_addr: addrs.param(),
                local_addr: addrs.param(),
            }),
            _ => Err(RefusedNoIdentity(()).into()),
        }
    }
}

impl ClientInfo {
    fn header_negotiated(&self) -> bool {
        self.alpn
            .as_ref()
            .map(|tls::NegotiatedProtocol(p)| p == transport_header::PROTOCOL)
            .unwrap_or(false)
    }
}

// === impl LocalTcp ===

impl Param<Remote<ClientAddr>> for LocalTcp {
    fn param(&self) -> Remote<ClientAddr> {
        self.client_addr
    }
}

impl Param<AllowPolicy> for LocalTcp {
    fn param(&self) -> AllowPolicy {
        self.policy.clone()
    }
}

impl Param<tls::ConditionalServerTls> for LocalTcp {
    fn param(&self) -> tls::ConditionalServerTls {
        tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: Some(self.client_id.clone()),
            negotiated_protocol: None,
        })
    }
}

// === impl AuthorizedLocalTcp ===

impl Param<Remote<ServerAddr>> for AuthorizedLocalTcp {
    fn param(&self) -> Remote<ServerAddr> {
        self.addr
    }
}

impl Param<transport::labels::Key> for AuthorizedLocalTcp {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::inbound_server(
            tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                client_id: Some(self.client_id.clone()),
                negotiated_protocol: None,
            }),
            self.addr.into(),
            self.permit.labels.server.clone(),
        )
    }
}

// === impl LocalHttp ===

impl Param<Remote<ServerAddr>> for LocalHttp {
    fn param(&self) -> Remote<ServerAddr> {
        self.addr
    }
}

impl Param<OrigDstAddr> for LocalHttp {
    fn param(&self) -> OrigDstAddr {
        self.client.local_addr
    }
}

impl Param<Remote<ClientAddr>> for LocalHttp {
    fn param(&self) -> Remote<ClientAddr> {
        self.client.client_addr
    }
}

impl Param<transport::labels::Key> for LocalHttp {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::inbound_server(
            tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                client_id: Some(self.client.client_id.clone()),
                negotiated_protocol: None,
            }),
            self.addr.into(),
            self.policy.server_label(),
        )
    }
}

impl svc::Param<policy::AllowPolicy> for LocalHttp {
    fn param(&self) -> policy::AllowPolicy {
        self.policy.clone()
    }
}

impl svc::Param<http::Version> for LocalHttp {
    fn param(&self) -> http::Version {
        match self.protocol {
            SessionProtocol::Http1 => http::Version::Http1,
            SessionProtocol::Http2 => http::Version::H2,
        }
    }
}

impl svc::Param<http::normalize_uri::DefaultAuthority> for LocalHttp {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        http::normalize_uri::DefaultAuthority(None)
    }
}

impl svc::Param<policy::ServerLabel> for LocalHttp {
    fn param(&self) -> policy::ServerLabel {
        self.policy.server_label()
    }
}

impl svc::Param<tls::ConditionalServerTls> for LocalHttp {
    fn param(&self) -> tls::ConditionalServerTls {
        tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: Some(self.client.client_id.clone()),
            negotiated_protocol: self.client.alpn.clone(),
        })
    }
}

// === impl GatewayTransportHeader ===

impl Param<transport::labels::Key> for GatewayTransportHeader {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::inbound_server(
            self.param(),
            self.client.local_addr.into(),
            self.policy.server_label(),
        )
    }
}

impl Param<policy::AllowPolicy> for GatewayTransportHeader {
    fn param(&self) -> policy::AllowPolicy {
        self.policy.clone()
    }
}

impl Param<OrigDstAddr> for GatewayTransportHeader {
    fn param(&self) -> OrigDstAddr {
        self.client.local_addr
    }
}

impl Param<Remote<ClientAddr>> for GatewayTransportHeader {
    fn param(&self) -> Remote<ClientAddr> {
        self.client.client_addr
    }
}

impl Param<tls::ConditionalServerTls> for GatewayTransportHeader {
    fn param(&self) -> tls::ConditionalServerTls {
        tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: Some(self.client.client_id.clone()),
            negotiated_protocol: self.client.alpn.clone(),
        })
    }
}

// === impl RefusedNoHeader ===

impl From<RefusedNoHeader> for Error {
    fn from(_: RefusedNoHeader) -> Error {
        Error::from(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "Non-transport-header connection refused",
        ))
    }
}

// === TlsParams ===

impl<T> ExtractParam<tls::server::Timeout, T> for TlsParams {
    #[inline]
    fn extract_param(&self, _: &T) -> tls::server::Timeout {
        self.timeout
    }
}

impl<T> ExtractParam<identity::Server, T> for TlsParams {
    #[inline]
    fn extract_param(&self, _: &T) -> identity::Server {
        self.identity.clone()
    }
}

impl<T> InsertParam<tls::ConditionalServerTls, T> for TlsParams {
    type Target = (tls::ConditionalServerTls, T);

    #[inline]
    fn insert_param(&self, tls: tls::ConditionalServerTls, target: T) -> Self::Target {
        (tls, target)
    }
}

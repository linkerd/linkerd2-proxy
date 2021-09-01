use crate::{policy, Inbound};
use linkerd_app_core::{
    io,
    proxy::identity::LocalCrtKey,
    svc::{self, ExtractParam, InsertParam, Param},
    tls,
    transport::{self, metrics::SensorIo, ClientAddr, OrigDstAddr, Remote},
    transport_header::{self, NewTransportHeaderServer, SessionProtocol, TransportHeader},
    Conditional, Error, NameAddr, Result,
};
use std::{convert::TryFrom, fmt::Debug};
use thiserror::Error;
use tracing::{debug_span, info_span};

#[derive(Clone, Debug)]
struct WithTransportHeaderAlpn(LocalCrtKey);

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Local {
    port: u16,
    client_id: tls::ClientId,
    permit: policy::Permit,
}

/// Gateway connections come in two variants: those with a transport header, and
/// legacy connections, without a transport header.
#[derive(Debug, Clone)]
pub enum GatewayConnection {
    TransportHeader(GatewayTransportHeader),
    Legacy(Legacy),
}

#[derive(Debug, Clone)]
pub struct GatewayTransportHeader {
    pub target: NameAddr,
    pub protocol: Option<SessionProtocol>,
    pub client: ClientInfo,
    pub policy: policy::AllowPolicy,
}

#[derive(Debug, Clone)]
pub struct Legacy {
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

type FwdIo<I> = SensorIo<io::PrefixedIo<tls::server::Io<I>>>;
pub type GatewayIo<I> = io::EitherIo<FwdIo<I>, SensorIo<tls::server::Io<I>>>;

#[derive(Clone)]
struct TlsParams {
    timeout: tls::server::Timeout,
    identity: Option<WithTransportHeaderAlpn>,
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
    pub(crate) fn push_direct<T, I, NSvc, G, GSvc>(
        self,
        policies: impl policy::CheckPolicy + Clone + Send + Sync + 'static,
        gateway: G,
    ) -> Inbound<svc::BoxNewTcp<T, I>>
    where
        T: Param<Remote<ClientAddr>> + Param<OrigDstAddr>,
        T: Clone + Send + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        N: svc::NewService<Local, Service = NSvc> + Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<FwdIo<I>, Response = ()> + Clone + Send + Sync + Unpin + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send + Unpin,
        G: svc::NewService<GatewayConnection, Service = GSvc>
            + Clone
            + Send
            + Sync
            + Unpin
            + 'static,
        GSvc: svc::Service<GatewayIo<I>, Response = ()> + Send + 'static,
        GSvc::Error: Into<Error>,
        GSvc::Future: Send,
    {
        self.map_stack(|config, rt, inner| {
            let detect_timeout = config.proxy.detect_protocol_timeout;

            inner
                .push(transport::metrics::NewServer::layer(
                    rt.metrics.proxy.transport.clone(),
                ))
                .instrument(|_: &_| debug_span!("opaque"))
                .check_new_service::<Local, _>()
                // When the transport header is present, it may be used for either local TCP
                // forwarding, or we may be processing an HTTP gateway connection. HTTP gateway
                // connections that have a transport header must provide a target name as a part of
                // the header.
                .push_switch(
                    {
                        let policies = policies.clone();
                        move |(h, client): (TransportHeader, ClientInfo)| -> Result<_> {
                            match h {
                                TransportHeader {
                                    port,
                                    name: None,
                                    protocol: None,
                                } => {
                                    // When the transport header targets an alternate port (but does
                                    // not identify an alternate target name), we check the new
                                    // target's policy to determine whether the client can access
                                    // it.
                                    let allow = policies.check_policy(OrigDstAddr(
                                        (client.local_addr.ip(), port).into(),
                                    ))?;
                                    let tls = tls::ConditionalServerTls::Some(
                                        tls::ServerTls::Established {
                                            client_id: Some(client.client_id.clone()),
                                            negotiated_protocol: client.alpn,
                                        },
                                    );
                                    let permit = allow.check_authorized(client.client_addr, &tls)?;
                                    Ok(svc::Either::A(Local { port, permit, client_id: client.client_id, }))
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
                                    let policy = policies.check_policy(client.local_addr)?;
                                    // let tls = tls::ConditionalServerTls::Some(
                                    //     tls::ServerTls::Established {
                                    //         client_id: Some(client.client_id.clone()),
                                    //         negotiated_protocol: client.alpn.clone(),
                                    //     },
                                    // );
                                    // let permit = allow.check_authorized(client.client_addr, &tls)?;
                                    Ok(svc::Either::B(GatewayTransportHeader {
                                        target: NameAddr::from((name, port)),
                                        protocol,
                                        client,
                                        policy,
                                    }))
                                }
                                TransportHeader {
                                    name: None,
                                    protocol: Some(_),
                                    ..
                                } => Err(RefusedNoTarget.into()),
                            }
                        }
                    },
                    // HTTP detection is not necessary in this case, since the transport
                    // header indicates the connection's HTTP version.
                    svc::stack(gateway.clone())
                        .push_on_service(svc::MapTargetLayer::new(io::EitherIo::Left))
                        .push_map_target(GatewayConnection::TransportHeader)
                        .push(transport::metrics::NewServer::layer(
                            rt.metrics.proxy.transport.clone(),
                        ))
                        .instrument(
                            |g: &GatewayTransportHeader| info_span!("gateway", dst = %g.target),
                        )
                        .check_new_service::<GatewayTransportHeader, io::PrefixedIo<tls::server::Io<I>>>()
                        .into_inner(),
                )
                // Use ALPN to determine whether a transport header should be read.
                //
                // When the transport header is not present, perform HTTP detection to
                // support legacy gateway clients.
                .push(NewTransportHeaderServer::layer(detect_timeout))
                .push_switch(
                    move |client: ClientInfo| -> Result<_> {
                        if client.header_negotiated() {
                            Ok(svc::Either::A(client))
                        } else {
                            // When we receive legacy connections with no transport headers, we must
                            // be receiving a gateway connection from an older client.  We check the
                            // gateway address's policy to determine whether the client is
                            // authorized to use this gateway.
                            let policy = policies.check_policy(client.local_addr)?;
                            Ok(svc::Either::B(Legacy { client, policy }))
                        }
                    },
                    // TODO(ver): Remove this after we have another stable release out with
                    // transport header support.
                    svc::stack(gateway)
                        .push_map_target(GatewayConnection::Legacy)
                        .push_on_service(svc::MapTargetLayer::new(io::EitherIo::Right))
                        .push(transport::metrics::NewServer::layer(
                            rt.metrics.proxy.transport.clone(),
                        ))
                        .instrument(|_: &Legacy| info_span!("gateway", legacy = true))
                        .check_new_service::<Legacy, tls::server::Io<I>>()
                        .into_inner(),
                )
                .check_new_service::<ClientInfo, tls::server::Io<I>>()
                // Build a ClientInfo target for each accepted connection. Refuse the
                // connection if it doesn't include an mTLS identity.
                .push_request_filter(ClientInfo::try_from)
                .push(svc::BoxNewService::layer())
                .push(tls::NewDetectTls::layer(TlsParams {
                    timeout: tls::server::Timeout(detect_timeout),
                    identity: rt.identity.clone().map(WithTransportHeaderAlpn),
                }))
                .check_new_service::<T, I>()
                .push_on_service(svc::BoxService::layer())
                .push(svc::BoxNewService::layer())
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

// === impl Local ===

impl Param<u16> for Local {
    fn param(&self) -> u16 {
        self.port
    }
}

impl Param<transport::labels::Key> for Local {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::inbound_server(
            tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                client_id: Some(self.client_id.clone()),
                negotiated_protocol: None,
            }),
            ([127, 0, 0, 1], self.port).into(),
            self.permit.server_labels.clone(),
            self.permit.authz_labels.clone(),
        )
    }
}

// === impl GatewayTransportHeader ===

impl Param<transport::labels::Key> for GatewayTransportHeader {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::inbound_server(
            self.param(),
            self.client.local_addr.into(),
            self.policy.server_labels(),
            Default::default(),
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

// === impl Legacy ===

impl Param<transport::labels::Key> for Legacy {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::inbound_server(
            tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                client_id: Some(self.client.client_id.clone()),
                negotiated_protocol: self.client.alpn.clone(),
            }),
            self.client.local_addr.into(),
            self.policy.server_labels(),
            Default::default(),
        )
    }
}

// === impl WithTransportHeaderAlpn ===

impl svc::Param<tls::server::Config> for WithTransportHeaderAlpn {
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

impl svc::Param<tls::LocalId> for WithTransportHeaderAlpn {
    fn param(&self) -> tls::LocalId {
        self.0.id().clone()
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

impl<T> ExtractParam<Option<WithTransportHeaderAlpn>, T> for TlsParams {
    #[inline]
    fn extract_param(&self, _: &T) -> Option<WithTransportHeaderAlpn> {
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

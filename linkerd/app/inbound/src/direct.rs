use crate::{target::TcpEndpoint, Inbound};
use linkerd_app_core::{
    io,
    proxy::identity::LocalCrtKey,
    svc::{self, stack::Param},
    tls,
    transport::{self, listen, metrics::SensorIo},
    transport_header::{self, NewTransportHeaderServer, SessionProtocol, TransportHeader},
    Conditional, Error, NameAddr, Never,
};
use std::{convert::TryFrom, fmt::Debug, net::SocketAddr};

#[derive(Clone, Debug)]
struct WithTransportHeaderAlpn(LocalCrtKey);

/// Creates I/O errors when a connection cannot be forwarded because no transport
/// header was present.
#[derive(Debug, Default)]
struct RefusedNoHeader;

#[derive(Debug)]
pub struct RefusedNoIdentity(());

#[derive(Debug)]
struct RefusedNoTarget;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GatewayConnection {
    Transported(Transported),
    Legacy(ClientInfo),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Transported {
    pub target: NameAddr,
    pub protocol: Option<SessionProtocol>,
    pub client: ClientInfo,
}

/// Client connections *must* have an identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientInfo {
    pub client_id: tls::ClientId,
    pub alpn: Option<tls::NegotiatedProtocol>,
    pub client_addr: SocketAddr,
    pub local_addr: SocketAddr,
}

type FwdIo<I> = io::PrefixedIo<SensorIo<tls::server::Io<I>>>;
pub type GatewayIo<I> = io::EitherIo<FwdIo<I>, SensorIo<tls::server::Io<I>>>;

impl<T> Inbound<T> {
    /// Builds a stack that handles connections that target the proxy's inbound port
    /// (i.e. without an SO_ORIGINAL_DST setting). This port behaves differently from
    /// the main proxy stack:
    ///
    /// 1. Protocol detection is always performed;
    /// 2. TLS is required;
    /// 3. A transport header is expected. It's not strictly required, as
    ///    gateways may need to accept HTTP requests from older proxy versions
    pub fn push_direct<I, TSvc, G, GSvc>(
        self,
        gateway: G,
    ) -> Inbound<
        impl svc::NewService<
                listen::Addrs,
                Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
            > + Clone,
    >
    where
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        T: svc::NewService<TcpEndpoint, Service = TSvc> + Clone + Send + Sync + Unpin + 'static,
        TSvc: svc::Service<FwdIo<I>, Response = ()> + Clone + Send + Sync + Unpin + 'static,
        TSvc::Error: Into<Error>,
        TSvc::Future: Send + Unpin,
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
        let Self {
            config,
            runtime: rt,
            stack: tcp,
        } = self;
        let detect_timeout = config.proxy.detect_protocol_timeout;

        let stack = tcp
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
                        protocol,
                    } => Ok(svc::Either::B(GatewayConnection::Transported(
                        Transported {
                            target: NameAddr::from((name, port)),
                            protocol,
                            client,
                        },
                    ))),
                    TransportHeader {
                        name: None,
                        protocol: Some(_),
                        ..
                    } => Err(RefusedNoTarget),
                },
                // HTTP detection is not necessary in this case, since the transport
                // header indicates the connection's HTTP version.
                svc::stack(gateway.clone())
                    .push_on_response(svc::layers().push_map_target(io::EitherIo::Left))
                    .check_new_service::<GatewayConnection, FwdIo<I>>()
                    .into_inner(),
            )
            // Use ALPN to determine whether a transport header should be read.
            //
            // When the transport header is not present, perform HTTP detection to
            // support legacy gateway clients.
            .push(NewTransportHeaderServer::layer(detect_timeout))
            .push_switch(
                |client: ClientInfo| {
                    if client.header_negotiated() {
                        Ok::<_, Never>(svc::Either::A(client))
                    } else {
                        Ok(svc::Either::B(GatewayConnection::Legacy(client)))
                    }
                },
                // TODO: Remove this after we have at least one stable release out
                // with transport header support.
                svc::stack(gateway)
                    .push_on_response(svc::layers().push_map_target(io::EitherIo::Right))
                    .check_new_service::<GatewayConnection, SensorIo<tls::server::Io<I>>>()
                    .into_inner(),
            )
            .check_new_service::<ClientInfo, SensorIo<tls::server::Io<I>>>()
            .push(rt.metrics.transport.layer_accept())
            // Build a ClientInfo target for each accepted connection. Refuse the
            // connection if it doesn't include an mTLS identity.
            .push_request_filter(ClientInfo::try_from)
            .push(tls::NewDetectTls::layer(
                rt.identity.clone().map(WithTransportHeaderAlpn),
                detect_timeout,
            ))
            .check_new_service::<listen::Addrs, I>();

        Inbound {
            config,
            runtime: rt,
            stack,
        }
    }
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
                negotiated_protocol,
            }) => Ok(Self {
                client_id,
                alpn: negotiated_protocol,
                client_addr: addrs.peer(),
                local_addr: addrs.target_addr(),
            }),
            _ => Err(RefusedNoIdentity(())),
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

impl Param<transport::labels::Key> for ClientInfo {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::accept(
            transport::labels::Direction::In,
            Conditional::Some(tls::ServerTls::Established {
                client_id: Some(self.client_id.clone()),
                negotiated_protocol: self.alpn.clone(),
            }),
            self.local_addr,
        )
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

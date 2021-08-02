use crate::{AllowPolicy, Inbound};
use linkerd_app_core::{
    detect, identity, io,
    proxy::{http, identity::LocalCrtKey},
    svc, tls,
    transport::{
        self,
        addrs::{ClientAddr, OrigDstAddr, Remote},
        ServerAddr,
    },
    Error, Infallible,
};
use std::fmt::Debug;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TlsAccept {
    client_addr: Remote<ClientAddr>,
    orig_dst_addr: OrigDstAddr,
    policy: AllowPolicy,
    tls: tls::ConditionalServerTls,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HttpAccept {
    inner: TlsAccept,
    http: http::Version,
}

#[derive(Clone)]
struct TlsParams {
    timeout: tls::server::Timeout,
    identity: Option<LocalCrtKey>,
}

#[derive(Debug, thiserror::Error)]
#[error("identity required on port {0}")]
struct IdentityRequired(u16);

// === impl Inbound ===

impl<N> Inbound<N> {
    /// Builds a stack that handles protocol detection according to the port's policy. If the
    /// connection is determined to be HTTP, the inner stack is used; otherwise the connection is
    /// passed to the provided 'forward' stack.
    pub fn push_detect<T, I, NSvc, F, FSvc>(self, forward: F) -> Inbound<svc::BoxNewTcp<T, I>>
    where
        T: svc::Param<OrigDstAddr> + svc::Param<Remote<ClientAddr>> + svc::Param<AllowPolicy>,
        T: Clone + Send + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        N: svc::NewService<HttpAccept, Service = NSvc> + Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<io::BoxedIo, Response = ()>,
        NSvc: Send + Unpin + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
        F: svc::NewService<TlsAccept, Service = FSvc> + Clone + Send + Sync + Unpin + 'static,
        FSvc: svc::Service<io::BoxedIo, Response = ()> + Clone + Send + 'static,
        FSvc::Error: Into<Error>,
        FSvc::Future: Send,
    {
        self.map_stack(|cfg, rt, http| {
            let detect_timeout = cfg.proxy.detect_protocol_timeout;

            http.check_new_service::<HttpAccept, _>()
                .push_map_target(|(http, inner)| HttpAccept { http, inner })
                .push(svc::UnwrapOr::layer(
                    // When HTTP detection fails, forward the connection to the application as
                    // an opaque TCP stream.
                    svc::stack(forward.clone()).into_inner(),
                ))
                .push_on_response(svc::MapTargetLayer::new(io::BoxedIo::new))
                .push(svc::BoxNewService::layer())
                .push_map_target(detect::allow_timeout)
                .push(detect::NewDetectService::layer(cfg.proxy.detect_http()))
                .check_new_service::<TlsAccept, _>()
                .push(rt.metrics.transport.layer_accept())
                .check_new_service::<TlsAccept, _>()
                .push_request_filter(|(tls, t): (tls::ConditionalServerTls, T)| {
                    match (t.param(), &tls) {
                        // Permit all connections if no authentication is required.
                        (AllowPolicy::Unauthenticated { .. }, _) => {
                            Ok(TlsAccept::from_params(&t, tls))
                        }
                        // Permit connections with a validated client identity if authentication
                        // is required.
                        (
                            AllowPolicy::Authenticated,
                            tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                                client_id: Some(_),
                                ..
                            }),
                        ) => Ok(TlsAccept::from_params(&t, tls)),
                        // Permit terminated TLS connections when client authentication is not
                        // required.
                        (
                            AllowPolicy::TlsUnauthenticated,
                            tls::ConditionalServerTls::Some(tls::ServerTls::Established { .. }),
                        ) => Ok(TlsAccept::from_params(&t, tls)),
                        // Otherwise, reject the connection.
                        _ => {
                            let OrigDstAddr(a) = t.param();
                            Err(IdentityRequired(a.port()))
                        }
                    }
                })
                .check_new_service::<(tls::ConditionalServerTls, T), _>()
                .push(svc::BoxNewService::layer())
                .push(tls::NewDetectTls::layer(TlsParams {
                    timeout: tls::server::Timeout(detect_timeout),
                    identity: rt.identity.clone(),
                }))
                .push_switch(
                    // If this port's policy indicates that authentication is not required and
                    // detection should be skipped, use the TCP stack directly.
                    |t: T| -> Result<_, Infallible> {
                        if let AllowPolicy::Unauthenticated { skip_detect: true } = t.param() {
                            return Ok(svc::Either::B(t));
                        }
                        Ok(svc::Either::A(t))
                    },
                    svc::stack(forward)
                        .push_on_response(svc::MapTargetLayer::new(io::BoxedIo::new))
                        .push_map_target(|t: T| TlsAccept {
                            client_addr: t.param(),
                            orig_dst_addr: t.param(),
                            policy: t.param(),
                            tls: tls::ConditionalServerTls::None(tls::NoServerTls::PortSkipped),
                        })
                        .into_inner(),
                )
                .push_on_response(svc::BoxService::layer())
                .push(svc::BoxNewService::layer())
        })
    }
}

// === impl TlsAccept ===

impl TlsAccept {
    fn from_params<T>(t: &T, tls: tls::ConditionalServerTls) -> Self
    where
        T: svc::Param<Remote<ClientAddr>> + svc::Param<OrigDstAddr> + svc::Param<AllowPolicy>,
    {
        Self {
            client_addr: t.param(),
            orig_dst_addr: t.param(),
            policy: t.param(),
            tls,
        }
    }
}

impl svc::Param<u16> for TlsAccept {
    fn param(&self) -> u16 {
        self.orig_dst_addr.as_ref().port()
    }
}

impl svc::Param<transport::labels::Key> for TlsAccept {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::Accept {
            direction: transport::labels::Direction::In,
            tls: self.tls.clone(),
            target_addr: self.orig_dst_addr.into(),
        }
    }
}

// === impl HttpAccept ===

impl svc::Param<http::Version> for HttpAccept {
    fn param(&self) -> http::Version {
        self.http
    }
}

impl svc::Param<Remote<ServerAddr>> for HttpAccept {
    fn param(&self) -> Remote<ServerAddr> {
        Remote(ServerAddr(self.inner.orig_dst_addr.into()))
    }
}

impl svc::Param<Remote<ClientAddr>> for HttpAccept {
    fn param(&self) -> Remote<ClientAddr> {
        self.inner.client_addr
    }
}

impl svc::Param<tls::ConditionalServerTls> for HttpAccept {
    fn param(&self) -> tls::ConditionalServerTls {
        self.inner.tls.clone()
    }
}

impl svc::Param<http::normalize_uri::DefaultAuthority> for HttpAccept {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        http::normalize_uri::DefaultAuthority(Some(
            std::str::FromStr::from_str(&self.inner.orig_dst_addr.to_string())
                .expect("Address must be a valid authority"),
        ))
    }
}

impl svc::Param<Option<identity::Name>> for HttpAccept {
    fn param(&self) -> Option<identity::Name> {
        self.inner
            .tls
            .value()
            .and_then(|server_tls| match server_tls {
                tls::ServerTls::Established {
                    client_id: Some(id),
                    ..
                } => Some(id.clone().0),
                _ => None,
            })
    }
}

// === TlsParams ===

impl<T> svc::ExtractParam<tls::server::Timeout, T> for TlsParams {
    #[inline]
    fn extract_param(&self, _: &T) -> tls::server::Timeout {
        self.timeout
    }
}

impl<T> svc::ExtractParam<Option<LocalCrtKey>, T> for TlsParams {
    #[inline]
    fn extract_param(&self, _: &T) -> Option<LocalCrtKey> {
        self.identity.clone()
    }
}

impl<T> svc::InsertParam<tls::ConditionalServerTls, T> for TlsParams {
    type Target = (tls::ConditionalServerTls, T);

    #[inline]
    fn insert_param(&self, tls: tls::ConditionalServerTls, target: T) -> Self::Target {
        (tls, target)
    }
}

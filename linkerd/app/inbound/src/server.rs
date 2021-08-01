use crate::{port_policies, Inbound};
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
use tracing::info_span;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Accept {
    client_addr: Remote<ClientAddr>,
    orig_dst_addr: OrigDstAddr,
    policy: port_policies::AllowPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TlsAccept {
    inner: Accept,
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
    /// Builds a stack that accepts connections. Connections to the proxy port are diverted to the
    /// 'direct' stack; otherwise connections are associated with a policy and passed to the inner
    /// stack.
    pub fn push_accept<T, I, NSvc, D, DSvc>(
        self,
        proxy_port: u16,
        direct: D,
    ) -> Inbound<svc::BoxNewTcp<T, I>>
    where
        T: svc::Param<Remote<ClientAddr>> + svc::Param<OrigDstAddr>,
        T: Clone + Send + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        N: svc::NewService<Accept, Service = NSvc> + Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<I, Response = ()>,
        NSvc: Send + Unpin + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
        D: svc::NewService<T, Service = DSvc> + Clone + Send + Sync + Unpin + 'static,
        DSvc: svc::Service<I, Response = ()> + Send + 'static,
        DSvc::Error: Into<Error>,
        DSvc::Future: Send,
    {
        self.map_stack(|cfg, rt, accept| {
            let port_policies = cfg.port_policies.clone();
            accept
                .push_switch(
                    move |t: T| -> Result<_, Error> {
                        let OrigDstAddr(addr) = t.param();
                        if addr.port() == proxy_port {
                            return Ok(svc::Either::B(t));
                        }
                        let policy = port_policies.check_allowed(addr.port())?;
                        Ok(svc::Either::A(Accept {
                            client_addr: t.param(),
                            orig_dst_addr: t.param(),
                            policy,
                        }))
                    },
                    direct,
                )
                .push(rt.metrics.tcp_accept_errors.layer())
                .instrument(|t: &T| {
                    let OrigDstAddr(addr) = t.param();
                    info_span!("server", port = addr.port())
                })
                .push_on_response(svc::BoxService::layer())
                .push(svc::BoxNewService::layer())
        })
    }

    /// Builds a stack that handles protocol detection according to the port's policy. If the
    /// connection is determined to be HTTP, the inner stack is used; otherwise the connection is
    /// passed to the provided 'forward' stack.
    pub fn push_detect<I, NSvc, F, FSvc>(self, forward: F) -> Inbound<svc::BoxNewTcp<Accept, I>>
    where
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
                .push_request_filter(|(tls, inner): (tls::ConditionalServerTls, Accept)| {
                    match (inner.policy, &tls) {
                        // Permit all connections if no authentication is required.
                        (port_policies::AllowPolicy::Unauthenticated { .. }, _) => {
                            Ok(TlsAccept { tls, inner })
                        }
                        // Permit connections with a validated client identity if authentication
                        // is required.
                        (
                            port_policies::AllowPolicy::Authenticated,
                            tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                                client_id: Some(_),
                                ..
                            }),
                        ) => Ok(TlsAccept { tls, inner }),
                        // Permit terminated TLS connections when client authentication is not
                        // required.
                        (
                            port_policies::AllowPolicy::TlsUnauthenticated,
                            tls::ConditionalServerTls::Some(tls::ServerTls::Established { .. }),
                        ) => Ok(TlsAccept { tls, inner }),
                        // Otherwise, reject the connection.
                        _ => Err(IdentityRequired(inner.orig_dst_addr.as_ref().port())),
                    }
                })
                .check_new_service::<(tls::ConditionalServerTls, Accept), _>()
                .push(svc::BoxNewService::layer())
                .push(tls::NewDetectTls::layer(TlsParams {
                    timeout: tls::server::Timeout(detect_timeout),
                    identity: rt.identity.clone(),
                }))
                .push_switch(
                    // If this port's policy indicates that authentication is not required and
                    // detection should be skipped, use the TCP stack directly.
                    |a: Accept| -> Result<_, Infallible> {
                        if let port_policies::AllowPolicy::Unauthenticated { skip_detect: true } =
                            a.policy
                        {
                            return Ok(svc::Either::B(a));
                        }
                        Ok(svc::Either::A(a))
                    },
                    svc::stack(forward)
                        .push_on_response(svc::MapTargetLayer::new(io::BoxedIo::new))
                        .push_map_target(|inner: Accept| TlsAccept {
                            inner,
                            tls: tls::ConditionalServerTls::None(tls::NoServerTls::PortSkipped),
                        })
                        .into_inner(),
                )
                .push_on_response(svc::BoxService::layer())
                .push(svc::BoxNewService::layer())
        })
    }
}

// === impl Accept ===

impl svc::Param<u16> for Accept {
    fn param(&self) -> u16 {
        self.orig_dst_addr.0.port()
    }
}

// === impl TlsAccept ===

impl svc::Param<u16> for TlsAccept {
    fn param(&self) -> u16 {
        self.inner.param()
    }
}

impl svc::Param<transport::labels::Key> for TlsAccept {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::Accept {
            direction: transport::labels::Direction::In,
            tls: self.tls.clone(),
            target_addr: self.inner.orig_dst_addr.into(),
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
        Remote(ServerAddr(self.inner.inner.orig_dst_addr.into()))
    }
}

impl svc::Param<Remote<ClientAddr>> for HttpAccept {
    fn param(&self) -> Remote<ClientAddr> {
        self.inner.inner.client_addr
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
            std::str::FromStr::from_str(&self.inner.inner.orig_dst_addr.to_string())
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

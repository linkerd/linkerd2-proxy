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
pub struct Tls {
    client_addr: Remote<ClientAddr>,
    orig_dst_addr: OrigDstAddr,
    policy: AllowPolicy,
    status: tls::ConditionalServerTls,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http {
    tls: Tls,
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
        N: svc::NewService<Http, Service = NSvc> + Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<io::BoxedIo, Response = ()>,
        NSvc: Send + Unpin + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
        F: svc::NewService<Tls, Service = FSvc> + Clone + Send + Sync + Unpin + 'static,
        FSvc: svc::Service<io::BoxedIo, Response = ()> + Send + 'static,
        FSvc::Error: Into<Error>,
        FSvc::Future: Send,
    {
        const TLS_PORT_SKIPPED: tls::ConditionalServerTls =
            tls::ConditionalServerTls::None(tls::NoServerTls::PortSkipped);

        self.map_stack(|cfg, rt, http| {
            let detect_timeout = cfg.proxy.detect_protocol_timeout;

            http.push_map_target(|(http, tls)| Http { http, tls })
                .push(svc::UnwrapOr::layer(
                    // When HTTP detection fails, forward the connection to the application as
                    // an opaque TCP stream.
                    forward.clone(),
                ))
                .push_on_response(svc::MapTargetLayer::new(io::BoxedIo::new))
                .push(svc::BoxNewService::layer())
                .push_map_target(detect::allow_timeout)
                .push(detect::NewDetectService::layer(cfg.proxy.detect_http()))
                .check_new_service::<Tls, _>()
                .push(rt.metrics.transport.layer_accept())
                .check_new_service::<Tls, _>()
                .push_request_filter(|(tls, t): (tls::ConditionalServerTls, T)| {
                    match (t.param(), &tls) {
                        // Permit all connections if no authentication is required.
                        (AllowPolicy::Unauthenticated { .. }, _) => Ok(Tls::from_params(&t, tls)),
                        // Permit connections with a validated client identity if authentication
                        // is required.
                        (
                            AllowPolicy::Authenticated,
                            tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                                client_id: Some(_),
                                ..
                            }),
                        ) => Ok(Tls::from_params(&t, tls)),
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
                        .push_map_target(|t: T| Tls::from_params(&t, TLS_PORT_SKIPPED))
                        .into_inner(),
                )
                .push_on_response(svc::BoxService::layer())
                .push(svc::BoxNewService::layer())
        })
    }
}

// === impl Tls ===

impl Tls {
    fn from_params<T>(t: &T, status: tls::ConditionalServerTls) -> Self
    where
        T: svc::Param<Remote<ClientAddr>> + svc::Param<OrigDstAddr> + svc::Param<AllowPolicy>,
    {
        Self {
            client_addr: t.param(),
            orig_dst_addr: t.param(),
            policy: t.param(),
            status,
        }
    }
}

impl svc::Param<u16> for Tls {
    fn param(&self) -> u16 {
        self.orig_dst_addr.as_ref().port()
    }
}

impl svc::Param<transport::labels::Key> for Tls {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::Accept {
            direction: transport::labels::Direction::In,
            tls: self.status.clone(),
            target_addr: self.orig_dst_addr.into(),
        }
    }
}

// === impl Http ===

impl svc::Param<http::Version> for Http {
    fn param(&self) -> http::Version {
        self.http
    }
}

impl svc::Param<Remote<ServerAddr>> for Http {
    fn param(&self) -> Remote<ServerAddr> {
        Remote(ServerAddr(self.tls.orig_dst_addr.into()))
    }
}

impl svc::Param<Remote<ClientAddr>> for Http {
    fn param(&self) -> Remote<ClientAddr> {
        self.tls.client_addr
    }
}

impl svc::Param<tls::ConditionalServerTls> for Http {
    fn param(&self) -> tls::ConditionalServerTls {
        self.tls.status.clone()
    }
}

impl svc::Param<http::normalize_uri::DefaultAuthority> for Http {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        http::normalize_uri::DefaultAuthority(Some(
            std::str::FromStr::from_str(&self.tls.orig_dst_addr.to_string())
                .expect("Address must be a valid authority"),
        ))
    }
}

impl svc::Param<Option<identity::Name>> for Http {
    fn param(&self) -> Option<identity::Name> {
        self.tls
            .status
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

// TODO figure out how to test authenticated requirements -- probably by splitting TLS detection.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util;
    use futures::future;
    use io::AsyncWriteExt;
    use linkerd_app_core::{
        svc::{NewService, ServiceExt},
        Error,
    };

    #[tokio::test(flavor = "current_thread")]
    async fn skip_detect() {
        let (io, _) = io::duplex(1);
        inbound()
            .with_stack(new_panic("detect stack must not be used"))
            .push_detect(new_ok())
            .into_inner()
            .new_service(Target(AllowPolicy::Unauthenticated { skip_detect: true }))
            .oneshot(io)
            .await
            .expect("should succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn non_http() {
        let (ior, mut iow) = io::duplex(100);
        iow.write_all(b"foo\r\nbar\r\nblah\r\n").await.unwrap();
        inbound()
            .with_stack(new_panic("http stack must not be used"))
            .push_detect(new_ok())
            .into_inner()
            .new_service(Target(AllowPolicy::Unauthenticated { skip_detect: false }))
            .oneshot(ior)
            .await
            .expect("should succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http() {
        let (ior, mut iow) = io::duplex(100);
        iow.write_all(b"GET / HTTP/1.1\r\nhost: example.com\r\n\r\n")
            .await
            .unwrap();
        inbound()
            .with_stack(new_ok())
            .push_detect(new_panic("tcp stack must not be used"))
            .into_inner()
            .new_service(Target(AllowPolicy::Unauthenticated { skip_detect: false }))
            .oneshot(ior)
            .await
            .expect("should succeed");
    }

    fn inbound() -> Inbound<()> {
        Inbound::new(test_util::default_config(), test_util::runtime().0)
    }

    fn new_panic<T>(msg: &'static str) -> svc::BoxNewTcp<T, io::BoxedIo> {
        svc::BoxNewService::new(move |_| panic!("{}", msg))
    }

    fn new_ok<T>() -> svc::BoxNewTcp<T, io::BoxedIo> {
        svc::BoxNewService::new(|_| svc::BoxService::new(svc::mk(|_| future::ok::<(), Error>(()))))
    }

    #[derive(Clone, Debug)]
    struct Target(AllowPolicy);

    impl svc::Param<AllowPolicy> for Target {
        fn param(&self) -> AllowPolicy {
            self.0
        }
    }

    impl svc::Param<OrigDstAddr> for Target {
        fn param(&self) -> OrigDstAddr {
            OrigDstAddr(([192, 0, 2, 2], 1000).into())
        }
    }

    impl svc::Param<Remote<ClientAddr>> for Target {
        fn param(&self) -> Remote<ClientAddr> {
            Remote(ClientAddr(([192, 0, 2, 3], 54321).into()))
        }
    }
}

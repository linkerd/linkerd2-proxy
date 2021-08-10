use crate::{
    port_policies::{AllowPolicy, DeniedUnauthorized, Permitted},
    Inbound,
};
use linkerd_app_core::{
    detect, identity, io,
    proxy::{http, identity::LocalCrtKey},
    svc, tls,
    transport::{
        self,
        addrs::{ClientAddr, OrigDstAddr, Remote},
        ServerAddr,
    },
    Error,
};
use std::fmt::Debug;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tls {
    client_addr: Remote<ClientAddr>,
    orig_dst_addr: OrigDstAddr,
    permit: Permitted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Http {
    tls: Tls,
    http: http::Version,
}

#[derive(Clone)]
struct TlsParams {
    timeout: tls::server::Timeout,
    identity: Option<LocalCrtKey>,
}

// === impl Inbound ===

impl<N> Inbound<N> {
    /// Builds a stack that handles TLS protocol detection according to the port's policy. If the
    /// connection is determined to be TLS, the inner stack is used; otherwise the connection is
    /// passed to the provided 'forward' stack.
    pub(crate) fn push_detect_tls<T, I, NSvc, F, FSvc>(
        self,
        forward: F,
    ) -> Inbound<svc::BoxNewTcp<T, I>>
    where
        T: svc::Param<OrigDstAddr> + svc::Param<Remote<ClientAddr>> + svc::Param<AllowPolicy>,
        T: Clone + Send + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        N: svc::NewService<Tls, Service = NSvc>,
        N: Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<tls::server::Io<I>, Response = ()>,
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

        self.map_stack(|cfg, rt, tls| {
            let detect_timeout = cfg.proxy.detect_protocol_timeout;
            tls.check_new_service::<Tls, tls::server::Io<I>>()
                .push_request_filter(
                    |(tls, t): (tls::ConditionalServerTls, T)| -> Result<Tls, DeniedUnauthorized> {
                        let policy: AllowPolicy = t.param();
                        let permit = policy.check_authorized(tls)?;
                        Ok(Tls::from_params(&t, permit))
                    },
                )
                .check_new_service::<(tls::ConditionalServerTls, T), tls::server::Io<I>>()
                .push(tls::NewDetectTls::layer(TlsParams {
                    timeout: tls::server::Timeout(detect_timeout),
                    identity: rt.identity.clone(),
                }))
                .check_new_service::<T, I>()
                .push_switch(
                    // If this port's policy indicates that authentication is not required and
                    // detection should be skipped, use the TCP stack directly.
                    |t: T| -> Result<_, DeniedUnauthorized> {
                        let policy: AllowPolicy = t.param();
                        if policy.is_opaque() {
                            let permit = policy.check_authorized(TLS_PORT_SKIPPED)?;
                            return Ok(svc::Either::B(Tls::from_params(&t, permit)));
                        }
                        Ok(svc::Either::A(t))
                    },
                    svc::stack(forward)
                        .push_on_response(svc::MapTargetLayer::new(io::BoxedIo::new))
                        .into_inner(),
                )
                .check_new_service::<T, I>()
                .push_on_response(svc::BoxService::layer())
                .push(svc::BoxNewService::layer())
        })
    }

    /// Builds a stack that handles HTTP detection once TLS detection has been
    /// performed. If the connection is determined to be HTTP, the inner stack
    /// is used; otherwise the connection is passed to the provided 'forward' stack.
    pub(crate) fn push_detect_http<I, NSvc, F, FSvc>(
        self,
        forward: F,
    ) -> Inbound<svc::BoxNewTcp<Tls, I>>
    where
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
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
        self.map_stack(|cfg, rt, http| {
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
                .push(rt.metrics.transport.layer_accept())
                .push_on_response(svc::BoxService::layer())
                .push(svc::BoxNewService::layer())
        })
    }
}

// === impl Tls ===

impl Tls {
    fn from_params<T>(t: &T, permit: Permitted) -> Self
    where
        T: svc::Param<Remote<ClientAddr>> + svc::Param<OrigDstAddr>,
    {
        Self {
            client_addr: t.param(),
            orig_dst_addr: t.param(),
            permit,
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
            tls: self.permit.tls.clone(),
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
        self.tls.permit.tls.clone()
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
            .permit
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util;
    use futures::future;
    use io::AsyncWriteExt;
    use linkerd_app_core::{
        svc::{NewService, ServiceExt},
        trace, Error,
    };
    use linkerd_server_policy::{Authentication, Authorization, Protocol, ServerPolicy};

    const HTTP: &[u8] = b"GET / HTTP/1.1\r\nhost: example.com\r\n\r\n";
    const NOT_HTTP: &[u8] = b"foo\r\nbar\r\nblah\r\n";

    #[tokio::test(flavor = "current_thread")]
    async fn detect_tls_opaque() {
        let _trace = trace::test::trace_init();

        let allow = AllowPolicy::new(
            client_addr(),
            orig_dst_addr(),
            ServerPolicy {
                protocol: Protocol::Opaque,
                authorizations: vec![Authorization {
                    authentication: Authentication::Unauthenticated,
                    networks: vec![ipnet::IpNet::from(client_addr().ip()).into()],
                    labels: None.into_iter().collect(),
                }],
                labels: None.into_iter().collect(),
            },
        );

        let (io, _) = io::duplex(1);
        inbound()
            .with_stack(new_panic("detect stack must not be used"))
            .push_detect_tls(new_ok())
            .into_inner()
            .new_service(Target(allow))
            .oneshot(io)
            .await
            .expect("should succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn detect_http_non_http() {
        let _trace = trace::test::trace_init();

        let target = Tls {
            client_addr: client_addr(),
            orig_dst_addr: orig_dst_addr(),
            permit: Permitted {
                protocol: Protocol::Detect {
                    timeout: std::time::Duration::from_secs(10),
                },
                labels: None.into_iter().collect(),
                tls: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                    client_id: Some(client_id()),
                    negotiated_protocol: None,
                }),
            },
        };

        let (ior, mut iow) = io::duplex(100);
        iow.write_all(NOT_HTTP).await.unwrap();

        inbound()
            .with_stack(new_panic("http stack must not be used"))
            .push_detect_http(new_ok())
            .into_inner()
            .new_service(target)
            .oneshot(ior)
            .await
            .expect("should succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn detect_http() {
        let _trace = trace::test::trace_init();

        let target = Tls {
            client_addr: client_addr(),
            orig_dst_addr: orig_dst_addr(),
            permit: Permitted {
                protocol: Protocol::Detect {
                    timeout: std::time::Duration::from_secs(10),
                },
                labels: None.into_iter().collect(),
                tls: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                    client_id: Some(client_id()),
                    negotiated_protocol: None,
                }),
            },
        };

        let (ior, mut iow) = io::duplex(100);
        iow.write_all(HTTP).await.unwrap();

        inbound()
            .with_stack(new_ok())
            .push_detect_http(new_panic("tcp stack must not be used"))
            .into_inner()
            .new_service(target)
            .oneshot(ior)
            .await
            .expect("should succeed");
    }

    fn client_id() -> tls::ClientId {
        "testsa.testns.serviceaccount.identity.linkerd.cluster.local"
            .parse()
            .unwrap()
    }

    fn client_addr() -> Remote<ClientAddr> {
        Remote(ClientAddr(([192, 0, 2, 3], 54321).into()))
    }

    fn orig_dst_addr() -> OrigDstAddr {
        OrigDstAddr(([192, 0, 2, 2], 1000).into())
    }

    fn inbound() -> Inbound<()> {
        Inbound::new(test_util::default_config(), test_util::runtime().0)
    }

    fn new_panic<T, I: 'static>(msg: &'static str) -> svc::BoxNewTcp<T, I> {
        svc::BoxNewService::new(move |_| -> svc::BoxTcp<I> { panic!("{}", msg) })
    }

    fn new_ok<T>() -> svc::BoxNewTcp<T, io::BoxedIo> {
        svc::BoxNewService::new(|_| svc::BoxService::new(svc::mk(|_| future::ok::<(), Error>(()))))
    }

    #[derive(Clone, Debug)]
    struct Target(AllowPolicy);

    impl svc::Param<AllowPolicy> for Target {
        fn param(&self) -> AllowPolicy {
            self.0.clone()
        }
    }

    impl svc::Param<OrigDstAddr> for Target {
        fn param(&self) -> OrigDstAddr {
            orig_dst_addr()
        }
    }

    impl svc::Param<Remote<ClientAddr>> for Target {
        fn param(&self) -> Remote<ClientAddr> {
            client_addr()
        }
    }
}

use crate::{
    policy::{AllowPolicy, Permit},
    Inbound,
};
use linkerd_app_core::{
    detect, identity, io,
    proxy::{http, identity::LocalCrtKey},
    svc, tls,
    transport::{
        self,
        addrs::{ClientAddr, OrigDstAddr, Remote},
        DeniedUnauthorized, ServerAddr,
    },
    Error, Infallible,
};
use linkerd_server_policy::Protocol;
use std::{fmt::Debug, time};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Tls {
    client_addr: Remote<ClientAddr>,
    orig_dst_addr: OrigDstAddr,
    permit: Permit,
}

#[derive(Clone, Debug)]
struct Detect {
    timeout: time::Duration,
    tls: Tls,
}

#[derive(Copy, Clone, Debug)]
struct ConfigureHttpDetect;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Http {
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
    /// Builds a stack that terminates mesh TLS and detects whether the traffic is HTTP (as hinted
    /// by policy).
    pub(crate) fn push_detect<T, I, NSvc, F, FSvc>(
        self,
        forward: F,
    ) -> Inbound<svc::BoxNewTcp<T, I>>
    where
        T: svc::Param<OrigDstAddr> + svc::Param<Remote<ClientAddr>> + svc::Param<AllowPolicy>,
        T: Clone + Send + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        N: svc::NewService<Http, Service = NSvc>,
        N: Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<io::BoxedIo, Response = ()>,
        NSvc: Send + Unpin + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
        F: svc::NewService<Tls, Service = FSvc> + Clone + Send + Sync + Unpin + 'static,
        FSvc: svc::Service<io::BoxedIo, Response = ()> + Send + 'static,
        FSvc::Error: Into<Error>,
        FSvc::Future: Send,
    {
        self.push_detect_http(forward.clone())
            .push_detect_tls(forward)
    }

    /// Builds a stack that handles TLS protocol detection according to the port's policy. If the
    /// connection is determined to be TLS, the inner stack is used; otherwise the connection is
    /// passed to the provided 'forward' stack.
    fn push_detect_tls<T, I, NSvc, F, FSvc>(self, forward: F) -> Inbound<svc::BoxNewTcp<T, I>>
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

        self.map_stack(|cfg, rt, detect| {
            let detect_timeout = cfg.proxy.detect_protocol_timeout;
            detect
                .push_switch(
                    // Ensure that the connection is authorized before proceeding with protocol
                    // detection.
                    |(tls, t): (tls::ConditionalServerTls, T)| -> Result<_, Error> {
                        let policy: AllowPolicy = t.param();

                        // If the port is configured to support application TLS, it may have also
                        // been wrapped in mesh identity. In any case, we don't actually validate
                        // whether app TLS was employed, but we use this as a signal that we should
                        // not perform additional protocol detection.
                        if policy.protocol() == Protocol::Tls {
                            let permit = policy.check_authorized(t.param(), tls)?;
                            return Ok(svc::Either::B(Tls::from_params(&t, permit)));
                        }

                        let permit = policy.check_authorized(t.param(), tls)?;
                        Ok(svc::Either::A(Tls::from_params(&t, permit)))
                    },
                    svc::stack(forward.clone())
                        .push_on_response(svc::MapTargetLayer::new(io::BoxedIo::new))
                        .into_inner(),
                )
                .push(tls::NewDetectTls::layer(TlsParams {
                    timeout: tls::server::Timeout(detect_timeout),
                    identity: rt.identity.clone(),
                }))
                .push_switch(
                    // If this port's policy indicates that authentication is not required and
                    // detection should be skipped, use the TCP stack directly.
                    |t: T| -> Result<_, DeniedUnauthorized> {
                        let policy: AllowPolicy = t.param();
                        if policy.protocol() == Protocol::Opaque {
                            let permit = policy.check_authorized(t.param(), TLS_PORT_SKIPPED)?;
                            return Ok(svc::Either::B(Tls::from_params(&t, permit)));
                        }
                        Ok(svc::Either::A(t))
                    },
                    svc::stack(forward)
                        .push_on_response(svc::MapTargetLayer::new(io::BoxedIo::new))
                        .into_inner(),
                )
                .push_on_response(svc::BoxService::layer())
                .push(svc::BoxNewService::layer())
        })
    }

    /// Builds a stack that handles HTTP detection once TLS detection has been performed. If the
    /// connection is determined to be HTTP, the inner stack is used; otherwise the connection is
    /// passed to the provided 'forward' stack.
    fn push_detect_http<I, NSvc, F, FSvc>(self, forward: F) -> Inbound<svc::BoxNewTcp<Tls, I>>
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
            let detect_timeout = cfg.proxy.detect_protocol_timeout;
            http.clone()
                .push_on_response(svc::MapTargetLayer::new(io::BoxedIo::new))
                .push_switch(
                    // If we have a protocol hint, skip detection and just used the hinted HTTP
                    // version.
                    move |tls: Tls| -> Result<_, Infallible> {
                        let http = match tls.permit.protocol {
                            Protocol::Detect { timeout } => {
                                return Ok(svc::Either::B(Detect { timeout, tls }));
                            }
                            Protocol::Http1 => {
                                // HTTP/1 services may actually be transported over HTTP/2
                                // connections between proxies, so we have to do detection.
                                return Ok(svc::Either::B(Detect {
                                    timeout: detect_timeout,
                                    tls,
                                }));
                            }
                            Protocol::Http2 | Protocol::Grpc => http::Version::H2,
                            _ => unreachable!("opaque protocols must not hit the HTTP stack"),
                        };
                        Ok(svc::Either::A(Http { http, tls }))
                    },
                    http.push_map_target(|(http, Detect { tls, .. })| Http { http, tls })
                        .push(svc::UnwrapOr::layer(
                            // When HTTP detection fails, forward the connection to the application as
                            // an opaque TCP stream.
                            svc::stack(forward.clone())
                                .push_map_target(|Detect { tls, .. }| tls)
                                .into_inner(),
                        ))
                        .push_on_response(svc::MapTargetLayer::new(io::BoxedIo::new))
                        .push(svc::BoxNewService::layer())
                        .push_map_target(detect::allow_timeout)
                        .push(detect::NewDetectService::layer(ConfigureHttpDetect)),
                )
                .push(transport::metrics::NewServer::layer(
                    rt.metrics.transport.clone(),
                ))
                .push_on_response(svc::BoxService::layer())
                .push(svc::BoxNewService::layer())
        })
    }
}

// === impl Tls ===

impl Tls {
    fn from_params<T>(t: &T, permit: Permit) -> Self
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
        transport::labels::Key::inbound_server(
            self.permit.tls.clone(),
            self.orig_dst_addr.into(),
            self.permit.server_labels.clone(),
            self.permit.authz_labels.clone(),
        )
    }
}

// === impl ConfigureHttpDetect ===

impl svc::ExtractParam<detect::Config<http::DetectHttp>, Detect> for ConfigureHttpDetect {
    fn extract_param(&self, detect: &Detect) -> detect::Config<http::DetectHttp> {
        detect::Config::from_timeout(detect.timeout)
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

impl svc::Param<Permit> for Http {
    fn param(&self) -> Permit {
        self.tls.permit.clone()
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

    const HTTP1: &[u8] = b"GET / HTTP/1.1\r\nhost: example.com\r\n\r\n";
    const HTTP2: &[u8] = b"PRI * HTTP/2.0\r\n";
    const NOT_HTTP: &[u8] = b"foo\r\nbar\r\nblah\r\n";

    #[tokio::test(flavor = "current_thread")]
    async fn detect_tls_opaque() {
        let _trace = trace::test::trace_init();

        let (allow, _tx) = AllowPolicy::for_test(
            orig_dst_addr(),
            ServerPolicy {
                protocol: Protocol::Opaque,
                authorizations: vec![Authorization {
                    authentication: Authentication::Unauthenticated,
                    networks: vec![client_addr().ip().into()],
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
            permit: Permit {
                protocol: Protocol::Detect {
                    timeout: std::time::Duration::from_secs(10),
                },
                tls: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                    client_id: Some(client_id()),
                    negotiated_protocol: None,
                }),
                server_labels: None.into_iter().collect(),
                authz_labels: None.into_iter().collect(),
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
            permit: Permit {
                protocol: Protocol::Detect {
                    timeout: std::time::Duration::from_secs(10),
                },
                tls: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                    client_id: Some(client_id()),
                    negotiated_protocol: None,
                }),
                server_labels: None.into_iter().collect(),
                authz_labels: None.into_iter().collect(),
            },
        };

        let (ior, mut iow) = io::duplex(100);
        iow.write_all(HTTP1).await.unwrap();

        inbound()
            .with_stack(new_ok())
            .push_detect_http(new_panic("tcp stack must not be used"))
            .into_inner()
            .new_service(target)
            .oneshot(ior)
            .await
            .expect("should succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hinted_http1() {
        let _trace = trace::test::trace_init();

        let target = Tls {
            client_addr: client_addr(),
            orig_dst_addr: orig_dst_addr(),
            permit: Permit {
                protocol: Protocol::Http1,
                tls: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                    client_id: Some(client_id()),
                    negotiated_protocol: None,
                }),
                server_labels: None.into_iter().collect(),
                authz_labels: None.into_iter().collect(),
            },
        };

        let (ior, mut iow) = io::duplex(100);
        iow.write_all(HTTP1).await.unwrap();

        inbound()
            .with_stack(new_ok())
            .push_detect_http(new_panic("tcp stack must not be used"))
            .into_inner()
            .new_service(target)
            .oneshot(ior)
            .await
            .expect("should succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hinted_http1_supports_http2() {
        let _trace = trace::test::trace_init();

        let target = Tls {
            client_addr: client_addr(),
            orig_dst_addr: orig_dst_addr(),
            permit: Permit {
                protocol: Protocol::Http1,
                tls: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                    client_id: Some(client_id()),
                    negotiated_protocol: None,
                }),
                server_labels: None.into_iter().collect(),
                authz_labels: None.into_iter().collect(),
            },
        };

        let (ior, mut iow) = io::duplex(100);
        iow.write_all(HTTP2).await.unwrap();

        inbound()
            .with_stack(new_ok())
            .push_detect_http(new_panic("tcp stack must not be used"))
            .into_inner()
            .new_service(target)
            .oneshot(ior)
            .await
            .expect("should succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hinted_http2() {
        let _trace = trace::test::trace_init();

        let target = Tls {
            client_addr: client_addr(),
            orig_dst_addr: orig_dst_addr(),
            permit: Permit {
                protocol: Protocol::Http2,
                tls: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                    client_id: Some(client_id()),
                    negotiated_protocol: None,
                }),
                server_labels: None.into_iter().collect(),
                authz_labels: None.into_iter().collect(),
            },
        };

        let (ior, _) = io::duplex(100);

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

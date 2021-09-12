use crate::{
    policy::{self, AllowPolicy, Permit, Protocol, ServerLabel},
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
    Error, Infallible,
};
use std::{fmt::Debug, time};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Forward {
    client_addr: Remote<ClientAddr>,
    orig_dst_addr: OrigDstAddr,
    tls: tls::ConditionalServerTls,
    permit: Permit,
}

#[derive(Clone, Debug)]
pub(crate) struct Http {
    tls: Tls,
    http: http::Version,
}

#[derive(Clone, Debug)]
struct Tls {
    client_addr: Remote<ClientAddr>,
    orig_dst_addr: OrigDstAddr,
    status: tls::ConditionalServerTls,
    policy: AllowPolicy,
}

#[derive(Clone, Debug)]
struct Detect {
    timeout: time::Duration,
    tls: Tls,
}

#[derive(Copy, Clone, Debug)]
struct ConfigureHttpDetect;

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
        F: svc::NewService<Forward, Service = FSvc> + Clone + Send + Sync + Unpin + 'static,
        FSvc: svc::Service<io::BoxedIo, Response = (), Error = Error> + Send + 'static,
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
        F: svc::NewService<Forward, Service = FSvc> + Clone + Send + Sync + Unpin + 'static,
        FSvc: svc::Service<io::BoxedIo, Response = (), Error = Error> + Send + 'static,
        FSvc::Future: Send,
    {
        self.map_stack(|cfg, rt, detect| {
            let forward = svc::stack(forward)
                .push_map_target(Forward::from)
                .push(policy::NewAuthorizeTcp::layer(rt.metrics.tcp_authz.clone()));

            let detect_timeout = cfg.proxy.detect_protocol_timeout;
            detect
                .check_new_service::<Tls, _>()
                .push_switch(
                    // Ensure that the connection is authorized before proceeding with protocol
                    // detection.
                    |(status, t): (tls::ConditionalServerTls, T)| -> Result<_, Infallible> {
                        let policy: AllowPolicy = t.param();
                        let protocol = policy.protocol();
                        let tls = Tls {
                            client_addr: t.param(),
                            orig_dst_addr: t.param(),
                            status,
                            policy,
                        };

                        // If the port is configured to support application TLS, it may have also
                        // been wrapped in mesh identity. In any case, we don't actually validate
                        // whether app TLS was employed, but we use this as a signal that we should
                        // not perform additional protocol detection.
                        if protocol == Protocol::Tls {
                            return Ok(svc::Either::B(tls));
                        }

                        Ok(svc::Either::A(tls))
                    },
                    forward
                        .clone()
                        .push_on_service(svc::MapTargetLayer::new(io::BoxedIo::new))
                        .into_inner(),
                )
                .check_new_service::<(tls::ConditionalServerTls, T), _>()
                .push(tls::NewDetectTls::layer(TlsParams {
                    timeout: tls::server::Timeout(detect_timeout),
                    identity: rt.identity.clone(),
                }))
                .check_new_service::<T, I>()
                .push_switch(
                    // If this port's policy indicates that authentication is not required and
                    // detection should be skipped, use the TCP stack directly.
                    |t: T| -> Result<_, Infallible> {
                        let policy: AllowPolicy = t.param();
                        if policy.protocol() == Protocol::Opaque {
                            const TLS_PORT_SKIPPED: tls::ConditionalServerTls =
                                tls::ConditionalServerTls::None(tls::NoServerTls::PortSkipped);
                            return Ok(svc::Either::B(Tls {
                                client_addr: t.param(),
                                orig_dst_addr: t.param(),
                                status: TLS_PORT_SKIPPED,
                                policy,
                            }));
                        }
                        Ok(svc::Either::A(t))
                    },
                    forward
                        .push_on_service(svc::MapTargetLayer::new(io::BoxedIo::new))
                        .into_inner(),
                )
                .check_new_service::<T, I>()
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
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
        F: svc::NewService<Forward, Service = FSvc> + Clone + Send + Sync + Unpin + 'static,
        FSvc: svc::Service<io::BoxedIo, Response = (), Error = Error> + Send + 'static,
        FSvc::Future: Send,
    {
        self.map_stack(|cfg, rt, http| {
            let detect_timeout = cfg.proxy.detect_protocol_timeout;

            let detect = http
                .clone()
                .push_on_service(svc::MapTargetLayer::new(io::BoxedIo::new))
                .push(transport::metrics::NewServer::layer(
                    rt.metrics.proxy.transport.clone(),
                ))
                .check_new_service::<Http, io::PrefixedIo<I>>()
                .push_switch(
                    |(http, Detect { tls, .. })| -> Result<_, Infallible> {
                        match http {
                            Some(http) => Ok(svc::Either::A(Http { http, tls })),
                            // When HTTP detection fails, forward the connection to the application as
                            // an opaque TCP stream.
                            None => Ok(svc::Either::B(tls)),
                        }
                    },
                    svc::stack(forward)
                        .push_on_service(svc::MapTargetLayer::new(io::BoxedIo::new))
                        .push(transport::metrics::NewServer::layer(
                            rt.metrics.proxy.transport.clone(),
                        ))
                        .push_map_target(Forward::from)
                        .push(policy::NewAuthorizeTcp::layer(rt.metrics.tcp_authz.clone()))
                        .into_inner(),
                )
                .push(svc::ArcNewService::layer())
                .push_map_target(detect::allow_timeout)
                .push(detect::NewDetectService::layer(ConfigureHttpDetect))
                .check_new_service::<Detect, I>();

            http.push_on_service(svc::MapTargetLayer::new(io::BoxedIo::new))
                .push(transport::metrics::NewServer::layer(
                    rt.metrics.proxy.transport.clone(),
                ))
                .check_new_service::<Http, I>()
                .push_switch(
                    // If we have a protocol hint, skip detection and just used the hinted HTTP
                    // version.
                    move |tls: Tls| -> Result<_, Infallible> {
                        let http = match tls.policy.protocol() {
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
                    detect.into_inner(),
                )
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
                .check_new_service::<Tls, I>()
        })
    }
}

// === impl Forward ===

impl From<(Permit, Tls)> for Forward {
    fn from((permit, tls): (Permit, Tls)) -> Self {
        Self {
            client_addr: tls.client_addr,
            orig_dst_addr: tls.orig_dst_addr,
            tls: tls.status,
            permit,
        }
    }
}

impl svc::Param<u16> for Forward {
    fn param(&self) -> u16 {
        self.orig_dst_addr.as_ref().port()
    }
}

impl svc::Param<transport::labels::Key> for Forward {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::inbound_server(
            self.tls.clone(),
            self.orig_dst_addr.into(),
            self.permit.labels.server.clone(),
        )
    }
}

// === impl Tls ===

impl svc::Param<AllowPolicy> for Tls {
    fn param(&self) -> AllowPolicy {
        self.policy.clone()
    }
}

impl svc::Param<OrigDstAddr> for Tls {
    fn param(&self) -> OrigDstAddr {
        self.orig_dst_addr
    }
}

impl svc::Param<Remote<ClientAddr>> for Tls {
    fn param(&self) -> Remote<ClientAddr> {
        self.client_addr
    }
}

impl svc::Param<tls::ConditionalServerTls> for Tls {
    fn param(&self) -> tls::ConditionalServerTls {
        self.status.clone()
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

impl svc::Param<OrigDstAddr> for Http {
    fn param(&self) -> OrigDstAddr {
        self.tls.orig_dst_addr
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

impl svc::Param<AllowPolicy> for Http {
    fn param(&self) -> AllowPolicy {
        self.tls.policy.clone()
    }
}

impl svc::Param<ServerLabel> for Http {
    fn param(&self) -> ServerLabel {
        self.tls.policy.server_label()
    }
}

impl svc::Param<transport::labels::Key> for Http {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::inbound_server(
            self.tls.status.clone(),
            self.tls.orig_dst_addr.into(),
            self.tls.policy.server_label(),
        )
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

    fn allow(protocol: Protocol) -> AllowPolicy {
        let (allow, _tx) = AllowPolicy::for_test(
            orig_dst_addr(),
            ServerPolicy {
                protocol,
                authorizations: vec![Authorization {
                    authentication: Authentication::Unauthenticated,
                    networks: vec![client_addr().ip().into()],
                    name: "testsaz".to_string(),
                }],
                name: "testsrv".to_string(),
            },
        );
        allow
    }

    #[tokio::test(flavor = "current_thread")]
    async fn detect_tls_opaque() {
        let _trace = trace::test::trace_init();

        let (io, _) = io::duplex(1);
        inbound()
            .with_stack(new_panic("detect stack must not be used"))
            .push_detect_tls(new_ok())
            .into_inner()
            .new_service(Target(allow(Protocol::Opaque)))
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
            status: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                client_id: Some(client_id()),
                negotiated_protocol: None,
            }),
            policy: allow(Protocol::Detect {
                timeout: std::time::Duration::from_secs(10),
            }),
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
            status: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                client_id: Some(client_id()),
                negotiated_protocol: None,
            }),
            policy: allow(Protocol::Detect {
                timeout: std::time::Duration::from_secs(10),
            }),
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
            status: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                client_id: Some(client_id()),
                negotiated_protocol: None,
            }),
            policy: allow(Protocol::Http1),
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
            status: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                client_id: Some(client_id()),
                negotiated_protocol: None,
            }),
            policy: allow(Protocol::Http1),
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
            status: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                client_id: Some(client_id()),
                negotiated_protocol: None,
            }),
            policy: allow(Protocol::Http2),
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
        svc::ArcNewService::new(move |_| -> svc::BoxTcp<I> { panic!("{}", msg) })
    }

    fn new_ok<T>() -> svc::BoxNewTcp<T, io::BoxedIo> {
        svc::ArcNewService::new(|_| svc::BoxService::new(svc::mk(|_| future::ok::<(), Error>(()))))
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

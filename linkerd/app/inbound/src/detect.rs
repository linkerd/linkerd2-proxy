use crate::{
    policy::{self, AllowPolicy, Protocol, ServerPermit},
    Inbound,
};
use linkerd_app_core::{
    identity, io,
    metrics::{prom, ServerLabel},
    proxy::http,
    svc, tls,
    transport::{
        self,
        addrs::{ClientAddr, OrigDstAddr, Remote},
        ServerAddr,
    },
    Error, Infallible,
};
use std::{fmt::Debug, time};
use tracing::info;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug)]
pub struct MetricsFamilies(pub HttpDetectMetrics);
pub type HttpDetectMetrics = http::DetectMetricsFamilies<ServerLabel>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Forward {
    client_addr: Remote<ClientAddr>,
    orig_dst_addr: OrigDstAddr,
    tls: tls::ConditionalServerTls,
    permit: ServerPermit,
}

#[derive(Clone, Debug)]
pub(crate) struct Http {
    tls: Tls,
    http: http::Variant,
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

#[derive(Clone)]
struct TlsParams {
    timeout: tls::server::Timeout,
    identity: identity::Server,
}

type TlsIo<I> = tls::server::Io<identity::ServerIo<tls::server::DetectIo<I>>, I>;

// === impl Inbound ===

impl Inbound<svc::ArcNewTcp<Http, io::BoxedIo>> {
    /// Builds a stack that terminates mesh TLS and detects whether the traffic is HTTP (as hinted
    /// by policy).
    pub(crate) fn push_detect<T, I, F, FSvc>(
        self,
        MetricsFamilies(metrics): MetricsFamilies,
        forward: F,
    ) -> Inbound<svc::ArcNewTcp<T, I>>
    where
        T: svc::Param<OrigDstAddr> + svc::Param<Remote<ClientAddr>> + svc::Param<AllowPolicy>,
        T: Clone + Send + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        F: svc::NewService<Forward, Service = FSvc> + Clone + Send + Sync + Unpin + 'static,
        FSvc: svc::Service<io::BoxedIo, Response = ()> + Send + 'static,
        FSvc::Error: Into<Error>,
        FSvc::Future: Send,
    {
        self.push_detect_http(metrics, forward.clone())
            .push_detect_tls(forward)
    }

    /// Builds a stack that handles HTTP detection once TLS detection has been performed. If the
    /// connection is determined to be HTTP, the inner stack is used; otherwise the connection is
    /// passed to the provided 'forward' stack.
    fn push_detect_http<I, F, FSvc>(
        self,
        metrics: HttpDetectMetrics,
        forward: F,
    ) -> Inbound<svc::ArcNewTcp<Tls, I>>
    where
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        F: svc::NewService<Forward, Service = FSvc> + Clone + Send + Sync + Unpin + 'static,
        FSvc: svc::Service<io::BoxedIo, Response = ()> + Send + 'static,
        FSvc::Error: Into<Error>,
        FSvc::Future: Send,
    {
        self.map_stack(|cfg, rt, http| {
            let forward = svc::stack(forward)
                .push_on_service(svc::MapTargetLayer::new(io::BoxedIo::new))
                .push(transport::metrics::NewServer::layer(
                    rt.metrics.proxy.transport.clone(),
                ))
                .push_map_target(Forward::from)
                .push(policy::NewTcpPolicy::layer(rt.metrics.tcp_authz.clone()))
                .arc_new_tcp();

            let detect_timeout = cfg.proxy.detect_protocol_timeout;
            let detect = http
                .clone()
                .push_on_service(svc::MapTargetLayer::new(io::BoxedIo::new))
                .push(transport::metrics::NewServer::layer(
                    rt.metrics.proxy.transport.clone(),
                ))
                .push_switch(
                    |(detected, Detect { tls, .. })| -> Result<_, Infallible> {
                        match detected {
                            http::Detection::Http(http) => Ok(svc::Either::A(Http { http, tls })),
                            http::Detection::NotHttp => Ok(svc::Either::B(tls)),
                            // When HTTP detection fails, forward the connection to the application as
                            // an opaque TCP stream.
                            http::Detection::ReadTimeout(timeout) => {
                                match tls.policy.protocol() {
                                    Protocol::Http1 { .. } => {
                                        // If the protocol was hinted to be HTTP/1.1 but detection
                                        // failed, we'll usually be handling HTTP/1, but we may actually
                                        // be handling HTTP/2 via protocol upgrade. Our options are:
                                        // handle the connection as HTTP/1, assuming it will be rare for
                                        // a proxy to initiate TLS, etc and not send the 16B of
                                        // connection header; or we can handle it as opaque--but there's
                                        // no chance the server will be able to handle the H2 protocol
                                        // upgrade. So, it seems best to assume it's HTTP/1 and let the
                                        // proxy handle the protocol error if we're in an edge case.
                                        info!(
                                            ?timeout,
                                            "Handling connection as HTTP/1 due to policy"
                                        );
                                        Ok(svc::Either::A(Http {
                                            http: http::Variant::Http1,
                                            tls,
                                        }))
                                    }
                                    // Otherwise, the protocol hint must have
                                    // been `Detect` or the protocol was updated
                                    // after detection was initiated, otherwise
                                    // we would have avoided detection below.
                                    // Continue handling the connection as if it
                                    // were opaque.
                                    _ => {
                                        info!(
                                            ?timeout,
                                            "Handling connection as opaque due to policy"
                                        );
                                        Ok(svc::Either::B(tls))
                                    }
                                }
                            }
                        }
                    },
                    forward.into_inner(),
                )
                .lift_new_with_target()
                .push(http::NewDetect::layer(
                    move |Detect { timeout, tls }: &Detect| http::DetectParams {
                        read_timeout: *timeout,
                        metrics: metrics.metrics(tls.policy.server_label()),
                    },
                ))
                .arc_new_tcp();

            http.push_on_service(svc::MapTargetLayer::new(io::BoxedIo::new))
                .push(transport::metrics::NewServer::layer(
                    rt.metrics.proxy.transport.clone(),
                ))
                .push_switch(
                    // If we have a protocol hint, skip detection and just used the hinted HTTP
                    // version.
                    move |tls: Tls| -> Result<_, Infallible> {
                        let http = match tls.policy.protocol() {
                            Protocol::Detect { timeout, .. } => {
                                return Ok(svc::Either::B(Detect { timeout, tls }));
                            }
                            // Meshed HTTP/1 services may actually be transported over HTTP/2 connections
                            // between proxies, so we have to do detection.
                            //
                            // TODO(ver) outbound clients should hint this with ALPN so we don't
                            // have to detect this situation.
                            Protocol::Http1 { .. } if tls.status.is_some() => {
                                return Ok(svc::Either::B(Detect {
                                    timeout: detect_timeout,
                                    tls,
                                }));
                            }
                            // Unmeshed services don't use protocol upgrading, so we can use the
                            // hint without further detection.
                            Protocol::Http1 { .. } => http::Variant::Http1,
                            Protocol::Http2 { .. } | Protocol::Grpc { .. } => http::Variant::H2,
                            _ => unreachable!("opaque protocols must not hit the HTTP stack"),
                        };
                        Ok(svc::Either::A(Http { http, tls }))
                    },
                    detect.into_inner(),
                )
                .arc_new_tcp()
        })
    }
}

impl<I> Inbound<svc::ArcNewTcp<Tls, TlsIo<I>>> {
    /// Builds a stack that handles TLS protocol detection according to the port's policy. If the
    /// connection is determined to be TLS, the inner stack is used; otherwise the connection is
    /// passed to the provided 'forward' stack.
    fn push_detect_tls<T, F, FSvc>(self, forward: F) -> Inbound<svc::ArcNewTcp<T, I>>
    where
        T: svc::Param<OrigDstAddr> + svc::Param<Remote<ClientAddr>> + svc::Param<AllowPolicy>,
        T: Clone + Send + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        F: svc::NewService<Forward, Service = FSvc> + Clone + Send + Sync + Unpin + 'static,
        FSvc: svc::Service<io::BoxedIo, Response = ()> + Send + 'static,
        FSvc::Error: Into<Error>,
        FSvc::Future: Send,
    {
        self.map_stack(|cfg, rt, detect| {
            let forward = svc::stack(forward)
                .push_on_service(svc::MapTargetLayer::new(io::BoxedIo::new))
                .push(transport::metrics::NewServer::layer(
                    rt.metrics.proxy.transport.clone(),
                ))
                .push_map_target(Forward::from)
                .push(policy::NewTcpPolicy::layer(rt.metrics.tcp_authz.clone()))
                .arc_new_tcp();

            let detect_timeout = cfg.proxy.detect_protocol_timeout;
            detect
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
                        if matches!(protocol, Protocol::Tls { .. }) {
                            return Ok(svc::Either::B(tls));
                        }

                        Ok(svc::Either::A(tls))
                    },
                    forward
                        .clone()
                        .push_on_service(svc::MapTargetLayer::new(io::BoxedIo::new))
                        .into_inner(),
                )
                .arc_new_tcp()
                .push(tls::NewDetectTls::<identity::Server, _, _>::layer(
                    TlsParams {
                        timeout: tls::server::Timeout(detect_timeout),
                        identity: rt.identity.server(),
                    },
                ))
                .arc_new_tcp()
                .push_switch(
                    // Check the policy for this port and check whether
                    // detection should occur. Policy is enforced on the forward
                    // or HTTP detection stack.
                    |t: T| -> Result<_, Infallible> {
                        let policy: AllowPolicy = t.param();
                        if matches!(policy.protocol(), Protocol::Opaque { .. }) {
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
                .arc_new_tcp()
        })
    }
}

// === impl Forward ===

impl From<(ServerPermit, Tls)> for Forward {
    fn from((permit, tls): (ServerPermit, Tls)) -> Self {
        Self {
            client_addr: tls.client_addr,
            orig_dst_addr: tls.orig_dst_addr,
            tls: tls.status,
            permit,
        }
    }
}

impl svc::Param<Remote<ServerAddr>> for Forward {
    fn param(&self) -> Remote<ServerAddr> {
        Remote(ServerAddr(self.orig_dst_addr.into()))
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

// === impl Http ===

impl svc::Param<http::Variant> for Http {
    fn param(&self) -> http::Variant {
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

impl svc::Param<Option<identity::Id>> for Http {
    fn param(&self) -> Option<identity::Id> {
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

impl<T> svc::ExtractParam<identity::Server, T> for TlsParams {
    #[inline]
    fn extract_param(&self, _: &T) -> identity::Server {
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

// === impl MetricsFamilies ===

impl MetricsFamilies {
    pub fn register(reg: &mut prom::Registry) -> Self {
        Self(http::DetectMetricsFamilies::register(
            reg.sub_registry_with_prefix("http"),
        ))
    }
}

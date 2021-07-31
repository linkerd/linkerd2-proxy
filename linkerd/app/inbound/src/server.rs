use crate::{direct, port_policies, target::TcpEndpoint, Inbound};
use linkerd_app_core::{
    detect, identity, io, profiles,
    proxy::{http, identity::LocalCrtKey},
    svc, tls,
    transport::addrs::{ClientAddr, OrigDstAddr, Remote},
    Error,
};
use std::fmt::Debug;
use tracing::{debug_span, info_span};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Accept {
    client_addr: Remote<ClientAddr>,
    orig_dst_addr: OrigDstAddr,
    policy: port_policies::AllowPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TlsAccept {
    inner: Accept,
    tls: tls::ConditionalServerTls,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct HttpAccept {
    inner: TlsAccept,
    http: http::Version,
}

#[derive(Clone)]
struct TlsParams {
    timeout: tls::server::Timeout,
    identity: Option<LocalCrtKey>,
}

// === impl Inbound ===

impl<C> Inbound<C> {
    pub fn push_server<T, I, G, GSvc, P>(
        self,
        proxy_port: u16,
        profiles: P,
        gateway: G,
    ) -> Inbound<svc::BoxNewTcp<T, I>>
    where
        C: svc::Service<TcpEndpoint> + Clone + Send + Sync + Unpin + 'static,
        C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
        C::Error: Into<Error>,
        C::Future: Send,
        T: svc::Param<Remote<ClientAddr>> + svc::Param<OrigDstAddr>,
        T: Clone + Send + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        G: svc::NewService<direct::GatewayConnection, Service = GSvc>,
        G: Clone + Send + Sync + Unpin + 'static,
        GSvc: svc::Service<direct::GatewayIo<I>, Response = ()> + Send + 'static,
        GSvc::Error: Into<Error>,
        GSvc::Future: Send,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + Unpin + 'static,
        P::Error: Send,
        P::Future: Send,
    {
        let tcp = self.clone();

        self.push_http_router(profiles)
            .push_http_server()
            .map_stack(|cfg, rt, http| {
                let detect_timeout = cfg.proxy.detect_protocol_timeout;

                http.check_new_service::<HttpAccept, _>()
                    .push_map_target(|(http, inner)| HttpAccept { http, inner })
                    .push(svc::UnwrapOr::layer(
                        // When HTTP detection fails, forward the connection to the application as
                        // an opaque TCP stream.
                        tcp.clone()
                            .push_tcp_forward()
                            .into_stack()
                            .push_map_target(TcpEndpoint::from_param)
                            .check_clone_new_service::<TlsAccept, _>()
                            .push_on_response(svc::BoxService::layer())
                            .into_inner(),
                    ))
                    .push(svc::BoxNewService::layer())
                    .push_map_target(detect::allow_timeout)
                    .push(detect::NewDetectService::layer(cfg.proxy.detect_http()))
                    .push(rt.metrics.transport.layer_accept())
                    .push_map_target(|(tls, inner)| TlsAccept { tls, inner })
                    .push(svc::BoxNewService::layer())
                    .push(tls::NewDetectTls::layer(TlsParams {
                        timeout: tls::server::Timeout(detect_timeout),
                        identity: rt.identity.clone(),
                    }))
            })
            .map_stack(|cfg, rt, accept| {
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
                        tcp.push_tcp_forward()
                            .push_direct(gateway)
                            .into_stack()
                            .instrument(|_: &_| debug_span!("direct"))
                            .into_inner(),
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
}

// === impl TlsAccept ===

impl svc::Param<u16> for TlsAccept {
    fn param(&self) -> u16 {
        self.inner.orig_dst_addr.0.port()
    }
}

// === impl HttpAccept ===

impl svc::Param<http::Version> for HttpAccept {
    fn param(&self) -> http::Version {
        self.http
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

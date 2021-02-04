use crate::{http, stack_labels, target::ShouldResolve, tcp};
use linkerd_app_core::{
    config::{ProxyConfig, ServerConfig},
    detect, discovery_rejected, drain, io, metrics, profiles, svc, Addr, Error, IpMatch,
};
use tracing::debug_span;

pub fn stack<T, TSvc, H, HSvc, I>(
    config: &ProxyConfig,
    metrics: &metrics::Proxy,
    drain: drain::Watch,
    tcp: T,
    http: H,
) -> impl svc::NewService<
    tcp::Logical,
    Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
> + Clone
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
    T: svc::NewService<(Option<Addr>, tcp::Logical), Service = TSvc> + Clone + Send + 'static,
    TSvc: svc::Service<io::EitherIo<I, io::PrefixedIo<I>>, Response = ()> + Send + 'static,
    TSvc::Error: Into<Error>,
    TSvc::Future: Send,
    H: svc::NewService<http::Logical, Service = HSvc> + Clone + Send + 'static,
    HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>,
    HSvc: Clone + Send + Sync + Unpin + 'static,
    HSvc::Error: Into<Error>,
    HSvc::Future: Send,
{
    let ProxyConfig {
        server: ServerConfig { h2_settings, .. },
        dispatch_timeout,
        detect_protocol_timeout,
        buffer_capacity,
        cache_max_idle_age,
        ..
    } = config.clone();

    let tcp_forward = svc::stack(tcp.clone())
        .push_map_target(|l| (None, l))
        .into_inner();

    svc::stack(http)
        .push_on_response(
            svc::layers()
                .push(http::BoxRequest::layer())
                .push(svc::MapErrLayer::new(Into::into)),
        )
        .push(http::NewServeHttp::layer(h2_settings, drain))
        .push_map_target(http::Logical::from)
        .push(svc::UnwrapOr::layer(
            // When an HTTP version cannot be detected, we fallback to a logical
            // TCP stack. This service needs to be buffered so that it can be
            // cached and cloned per connection.
            svc::stack(tcp.clone())
                .push(profiles::split::layer())
                .push_switch(ShouldResolve, tcp_forward.clone())
                .push_on_response(
                    svc::layers()
                        .push_map_target(io::EitherIo::Right)
                        .push(metrics.stack.layer(stack_labels("tcp", "logical")))
                        .push(svc::layer::mk(svc::SpawnReady::new))
                        .push(svc::FailFast::layer("TCP Logical", dispatch_timeout))
                        .push_spawn_buffer(buffer_capacity),
                )
                .instrument(|_: &_| debug_span!("tcp"))
                .check_new_service::<tcp::Logical, _>()
                .into_inner(),
        ))
        .push_cache(cache_max_idle_age)
        .push(detect::NewDetectService::layer(
            detect_protocol_timeout,
            http::DetectHttp::default(),
        ))
        .check_new_service::<tcp::Logical, _>()
        .push_switch(
            SkipByProfile,
            // When the profile marks the target as opaque, we skip HTTP
            // detection and just use the TCP logical stack directly. Unlike the
            // above case, this stack need not be buffered, since `fn cache`
            // applies its own buffer on the returned service.
            svc::stack(tcp)
                .push(profiles::split::layer())
                .push_switch(ShouldResolve, tcp_forward)
                .push_on_response(
                    svc::layers()
                        .push_map_target(io::EitherIo::Left)
                        .push(metrics.stack.layer(stack_labels("tcp", "passthru"))),
                )
                .instrument(|_: &_| debug_span!("tcp.opaque"))
                .into_inner(),
        )
        .check_new_service::<tcp::Logical, _>()
        .into_inner()
}

#[derive(Clone, Debug)]
pub struct AllowProfile(pub IpMatch);

// === impl AllowProfile ===

impl svc::stack::Predicate<tcp::Accept> for AllowProfile {
    type Request = std::net::SocketAddr;

    fn check(&mut self, a: tcp::Accept) -> Result<std::net::SocketAddr, Error> {
        if self.0.matches(a.orig_dst.ip()) {
            Ok(a.orig_dst)
        } else {
            Err(discovery_rejected().into())
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct SkipByProfile;

// === impl SkipByProfile ===

impl svc::Predicate<tcp::Logical> for SkipByProfile {
    type Request = svc::Either<tcp::Logical, tcp::Logical>;

    fn check(&mut self, l: tcp::Logical) -> Result<Self::Request, Error> {
        if l.profile
            .as_ref()
            .map(|p| !p.borrow().opaque_protocol)
            .unwrap_or(true)
        {
            Ok(svc::Either::A(l))
        } else {
            Ok(svc::Either::B(l))
        }
    }
}

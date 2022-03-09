use super::{NewRequireIdentity, NewStripProxyError, ProxyConnectionClose};
use crate::{tcp::opaque_transport, Outbound};
use linkerd_app_core::{
    classify, config, errors, http_tracing, metrics,
    proxy::{http, tap},
    svc::{self, ExtractParam},
    tls,
    transport::{self, Remote, ServerAddr},
    transport_header::SessionProtocol,
    Error, Result, CANONICAL_DST_HEADER,
};
use tokio::io;

#[derive(Copy, Clone, Debug)]
struct ClientRescue {
    emit_headers: bool,
}

#[derive(Clone, Debug)]
pub struct Connect<T> {
    version: http::Version,
    inner: T,
}

impl<C> Outbound<C> {
    pub fn push_http_endpoint<T, B>(self) -> Outbound<svc::ArcNewHttp<T, B>>
    where
        T: Clone + Send + Sync + 'static,
        T: svc::Param<http::client::Settings>
            + svc::Param<Option<http::AuthorityOverride>>
            + svc::Param<metrics::EndpointLabels>
            + svc::Param<tls::ConditionalClientTls>
            + tap::Inspect,
        B: http::HttpBody<Error = Error> + std::fmt::Debug + Default + Send + 'static,
        B::Data: Send + 'static,
        C: svc::Service<Connect<T>> + Clone + Send + Sync + Unpin + 'static,
        C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin,
        C::Error: Into<Error>,
        C::Future: Send + Unpin + 'static,
    {
        self.map_stack(|config, rt, connect| {
            let config::ConnectConfig {
                h1_settings,
                h2_settings,
                backoff,
                ..
            } = config.proxy.connect;

            // Initiates an HTTP client on the underlying transport. Prior-knowledge HTTP/2
            // is typically used (i.e. when communicating with other proxies); though
            // HTTP/1.x fallback is supported as needed.
            svc::stack(connect.into_inner())
                .check_service::<Connect<T>>()
                .push_map_target(|(version, inner)| Connect { version, inner })
                .push(http::client::layer(h1_settings, h2_settings))
                .push_on_service(svc::MapErr::layer(Into::<Error>::into))
                .check_service::<T>()
                .into_new_service()
                // Drive the connection to completion regardless of whether the reconnect is being
                // actively polled.
                .push_on_service(svc::layer::mk(svc::SpawnReady::new))
                .push_new_reconnect(backoff)
                // Set the TLS status on responses so that the stack can detect whether the request
                // was sent over a meshed connection.
                .push_http_response_insert_target::<tls::ConditionalClientTls>()
                // If the outbound proxy is not configured to emit headers, then strip the
                // `l5d-proxy-errors` header if set by the peer.
                .push(NewStripProxyError::layer(config.emit_headers))
                // Tear down server connections when a peer proxy generates an error.
                // TODO(ver) this should only be honored when forwarding and not when the connection
                // is part of a balancer.
                .push(ProxyConnectionClose::layer())
                // Handle connection-level errors eagerly so that we can report 5XX failures in tap
                // and metrics. HTTP error metrics are not incremented here so that errors are not
                // double-counted--i.e., endpoint metrics track these responses and error metrics
                // track proxy errors that occur higher in the stack.
                .push(ClientRescue::layer(config.emit_headers))
                .push(tap::NewTapHttp::layer(rt.tap.clone()))
                .push(
                    rt.metrics
                        .proxy
                        .http_endpoint
                        .to_layer::<classify::Response, _, _>(),
                )
                .push_on_service(http_tracing::client(
                    rt.span_sink.clone(),
                    crate::trace_labels(),
                ))
                .push(NewRequireIdentity::layer())
                .push(http::NewOverrideAuthority::layer(vec![
                    "host",
                    CANONICAL_DST_HEADER,
                ]))
                .push_on_service(
                    svc::layers()
                        .push(http::BoxResponse::layer())
                        .push(svc::BoxService::layer()),
                )
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl ClientRescue ===

impl ClientRescue {
    /// Synthesizes responses for HTTP requests that encounter proxy errors.
    pub fn layer<N>(
        emit_headers: bool,
    ) -> impl svc::layer::Layer<N, Service = errors::NewRespondService<Self, Self, N>> + Clone {
        errors::respond::layer(Self { emit_headers })
    }
}

impl<T> ExtractParam<Self, T> for ClientRescue {
    #[inline]
    fn extract_param(&self, _: &T) -> Self {
        *self
    }
}

impl<T> ExtractParam<errors::respond::EmitHeaders, T> for ClientRescue {
    #[inline]
    fn extract_param(&self, _: &T) -> errors::respond::EmitHeaders {
        // Always emit informational headers on responses to an application.
        errors::respond::EmitHeaders(self.emit_headers)
    }
}

impl errors::HttpRescue<Error> for ClientRescue {
    fn rescue(&self, error: Error) -> Result<errors::SyntheticHttpResponse> {
        let cause = errors::root_cause(&*error);
        if cause.is::<http::orig_proto::DowngradedH2Error>() {
            return Ok(errors::SyntheticHttpResponse::bad_gateway(cause));
        }
        if cause.is::<std::io::Error>() {
            return Ok(errors::SyntheticHttpResponse::bad_gateway(cause));
        }
        if cause.is::<errors::ConnectTimeout>() {
            return Ok(errors::SyntheticHttpResponse::gateway_timeout(cause));
        }

        Err(error)
    }
}

// === impl Connect ===

impl<T> svc::Param<Option<SessionProtocol>> for Connect<T> {
    #[inline]
    fn param(&self) -> Option<SessionProtocol> {
        match self.version {
            http::Version::Http1 => Some(SessionProtocol::Http1),
            http::Version::H2 => Some(SessionProtocol::Http2),
        }
    }
}

impl<T: svc::Param<Remote<ServerAddr>>> svc::Param<Remote<ServerAddr>> for Connect<T> {
    #[inline]
    fn param(&self) -> Remote<ServerAddr> {
        self.inner.param()
    }
}

impl<T: svc::Param<tls::ConditionalClientTls>> svc::Param<tls::ConditionalClientTls>
    for Connect<T>
{
    #[inline]
    fn param(&self) -> tls::ConditionalClientTls {
        self.inner.param()
    }
}

impl<T: svc::Param<Option<opaque_transport::PortOverride>>>
    svc::Param<Option<opaque_transport::PortOverride>> for Connect<T>
{
    #[inline]
    fn param(&self) -> Option<opaque_transport::PortOverride> {
        self.inner.param()
    }
}

impl<T: svc::Param<Option<http::AuthorityOverride>>> svc::Param<Option<http::AuthorityOverride>>
    for Connect<T>
{
    #[inline]
    fn param(&self) -> Option<http::AuthorityOverride> {
        self.inner.param()
    }
}

impl<T: svc::Param<transport::labels::Key>> svc::Param<transport::labels::Key> for Connect<T> {
    #[inline]
    fn param(&self) -> transport::labels::Key {
        self.inner.param()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{http, test_util::*};
    use ::http::header::{CONNECTION, UPGRADE};
    use linkerd_app_core::{
        io,
        proxy::api_resolve::Metadata,
        svc::{NewService, ServiceExt},
        Infallible,
    };
    use std::net::SocketAddr;
    use support::resolver::ProtocolHint;

    static WAS_ORIG_PROTO: &str = "request-orig-proto";

    /// Tests that the the HTTP endpoint stack forwards connections without HTTP upgrading.
    #[tokio::test(flavor = "current_thread")]
    async fn http11_forward() {
        let _trace = linkerd_tracing::test::trace_init();

        let addr = SocketAddr::new([192, 0, 2, 41].into(), 2041);

        let connect = support::connect()
            .endpoint_fn_boxed(addr, |_: http::Connect| serve(::http::Version::HTTP_11));

        // Build the outbound server
        let (rt, _shutdown) = runtime();
        let stack = Outbound::new(default_config(), rt)
            .with_stack(connect)
            .push_http_endpoint::<_, http::BoxBody>()
            .into_inner();

        let svc = stack.new_service(http::Endpoint {
            addr: Remote(ServerAddr(addr)),
            protocol: http::Version::Http1,
            logical_addr: None,
            opaque_protocol: false,
            tls: tls::ConditionalClientTls::None(tls::NoClientTls::Disabled),
            metadata: Metadata::default(),
        });

        let req = http::Request::builder()
            .version(::http::Version::HTTP_11)
            .uri("http://foo.example.com")
            .extension(http::ClientHandle::new(([192, 0, 2, 101], 40200).into()).0)
            .body(http::BoxBody::default())
            .unwrap();
        let rsp = svc.oneshot(req).await.unwrap();
        assert_eq!(rsp.status(), http::StatusCode::NO_CONTENT);
        assert!(rsp.headers().get(WAS_ORIG_PROTO).is_none());
    }

    /// Tests that the the HTTP endpoint stack forwards connections without HTTP upgrading.
    #[tokio::test(flavor = "current_thread")]
    async fn http2_forward() {
        let _trace = linkerd_tracing::test::trace_init();

        let addr = SocketAddr::new([192, 0, 2, 41].into(), 2042);

        let connect = support::connect()
            .endpoint_fn_boxed(addr, |_: http::Connect| serve(::http::Version::HTTP_2));

        // Build the outbound server
        let (rt, _shutdown) = runtime();
        let stack = Outbound::new(default_config(), rt)
            .with_stack(connect)
            .push_http_endpoint::<_, http::BoxBody>()
            .into_inner();

        let svc = stack.new_service(http::Endpoint {
            addr: Remote(ServerAddr(addr)),
            protocol: http::Version::H2,
            logical_addr: None,
            opaque_protocol: false,
            tls: tls::ConditionalClientTls::None(tls::NoClientTls::Disabled),
            metadata: Metadata::default(),
        });

        let req = http::Request::builder()
            .version(::http::Version::HTTP_2)
            .uri("http://foo.example.com")
            .extension(http::ClientHandle::new(([192, 0, 2, 101], 40200).into()).0)
            .body(http::BoxBody::default())
            .unwrap();
        let rsp = svc.oneshot(req).await.unwrap();
        assert_eq!(rsp.status(), http::StatusCode::NO_CONTENT);
        assert!(rsp.headers().get(WAS_ORIG_PROTO).is_none());
    }

    /// Tests that the the HTTP endpoint stack uses endpoint metadata to initiate HTTP/2 protocol
    /// upgrading.
    #[tokio::test(flavor = "current_thread")]
    async fn orig_proto_upgrade() {
        let _trace = linkerd_tracing::test::trace_init();

        let addr = SocketAddr::new([192, 0, 2, 41].into(), 2041);

        // Pretend the upstream is a proxy that supports proto upgrades...
        let connect = support::connect()
            .endpoint_fn_boxed(addr, |_: http::Connect| serve(::http::Version::HTTP_2));

        // Build the outbound server
        let (rt, _shutdown) = runtime();
        let stack = Outbound::new(default_config(), rt)
            .with_stack(connect)
            .push_http_endpoint::<_, http::BoxBody>()
            .into_inner();

        let svc = stack.new_service(http::Endpoint {
            addr: Remote(ServerAddr(addr)),
            protocol: http::Version::Http1,
            logical_addr: None,
            opaque_protocol: false,
            tls: tls::ConditionalClientTls::None(tls::NoClientTls::Disabled),
            metadata: Metadata::new(None, ProtocolHint::Http2, None, None, None),
        });

        let req = http::Request::builder()
            .version(::http::Version::HTTP_11)
            .uri("http://foo.example.com")
            .extension(http::ClientHandle::new(([192, 0, 2, 101], 40200).into()).0)
            .body(http::BoxBody::default())
            .unwrap();
        let rsp = svc.oneshot(req).await.unwrap();
        assert_eq!(rsp.status(), http::StatusCode::NO_CONTENT);
        assert_eq!(
            rsp.headers()
                .get(WAS_ORIG_PROTO)
                .and_then(|v| v.to_str().ok()),
            Some("HTTP/1.1")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn orig_proto_skipped_on_http_upgrade() {
        let _trace = linkerd_tracing::test::trace_init();

        let addr = SocketAddr::new([192, 0, 2, 41].into(), 2041);

        // Pretend the upstream is a proxy that supports proto upgrades. The service needs to
        // support both HTTP/1 and HTTP/2 because an HTTP/2 connection is maintained by default and
        // HTTP/1 connections are created as-needed.
        let connect = support::connect().endpoint_fn_boxed(addr, |c: http::Connect| {
            serve(match svc::Param::param(&c) {
                Some(SessionProtocol::Http1) => ::http::Version::HTTP_11,
                Some(SessionProtocol::Http2) => ::http::Version::HTTP_2,
                None => unreachable!(),
            })
        });

        // Build the outbound server
        let (rt, _shutdown) = runtime();
        let drain = rt.drain.clone();
        let stack = Outbound::new(default_config(), rt)
            .with_stack(connect)
            .push_http_endpoint::<_, http::BoxBody>()
            .into_stack()
            .push_on_service(http::BoxRequest::layer())
            // We need the server-side upgrade layer to annotate the request so that the client
            // knows that an HTTP upgrade is in progress.
            .push_on_service(svc::layer::mk(|svc| {
                http::upgrade::Service::new(svc, drain.clone())
            }))
            .into_inner();

        let svc = stack.new_service(http::Endpoint {
            addr: Remote(ServerAddr(addr)),
            protocol: http::Version::Http1,
            logical_addr: None,
            opaque_protocol: false,
            tls: tls::ConditionalClientTls::None(tls::NoClientTls::Disabled),
            metadata: Metadata::new(None, ProtocolHint::Http2, None, None, None),
        });

        let req = http::Request::builder()
            .version(::http::Version::HTTP_11)
            .uri("http://foo.example.com")
            .extension(http::ClientHandle::new(([192, 0, 2, 101], 40200).into()).0)
            // The request has upgrade headers
            .header(CONNECTION, "upgrade")
            .header(UPGRADE, "linkerdrocks")
            .body(hyper::Body::default())
            .unwrap();
        let rsp = svc.oneshot(req).await.unwrap();
        assert_eq!(rsp.status(), http::StatusCode::NO_CONTENT);
        // The request did NOT get a linkerd upgrade header.
        assert!(rsp.headers().get(WAS_ORIG_PROTO).is_none());
        assert_eq!(rsp.version(), ::http::Version::HTTP_11);
    }

    /// Tests that the the HTTP endpoint stack ignores protocol upgrade hinting for HTTP/2 traffic.
    #[tokio::test(flavor = "current_thread")]
    async fn orig_proto_http2_noop() {
        let _trace = linkerd_tracing::test::trace_init();

        let addr = SocketAddr::new([192, 0, 2, 41].into(), 2041);

        // Pretend the upstream is a proxy that supports proto upgrades...
        let connect = support::connect()
            .endpoint_fn_boxed(addr, |_: http::Connect| serve(::http::Version::HTTP_2));

        // Build the outbound server
        let (rt, _shutdown) = runtime();
        let stack = Outbound::new(default_config(), rt)
            .with_stack(connect)
            .push_http_endpoint::<_, http::BoxBody>()
            .into_inner();

        let svc = stack.new_service(http::Endpoint {
            addr: Remote(ServerAddr(addr)),
            protocol: http::Version::H2,
            logical_addr: None,
            opaque_protocol: false,
            tls: tls::ConditionalClientTls::None(tls::NoClientTls::Disabled),
            metadata: Metadata::new(None, ProtocolHint::Http2, None, None, None),
        });

        let req = http::Request::builder()
            .version(::http::Version::HTTP_2)
            .uri("http://foo.example.com")
            .extension(http::ClientHandle::new(([192, 0, 2, 101], 40200).into()).0)
            .body(http::BoxBody::default())
            .unwrap();
        let rsp = svc.oneshot(req).await.unwrap();
        assert_eq!(rsp.status(), http::StatusCode::NO_CONTENT);
        assert!(rsp.headers().get(WAS_ORIG_PROTO).is_none());
    }

    /// Helper server that reads the l5d-orig-proto header on requests and uses it to set the header
    /// value in `WAS_ORIG_PROTO`.
    fn serve(version: ::http::Version) -> io::Result<io::BoxedIo> {
        let svc = hyper::service::service_fn(move |req: http::Request<_>| {
            tracing::debug!(?req);
            let rsp = http::Response::builder()
                .version(version)
                .status(http::StatusCode::NO_CONTENT);
            let rsp = req
                .headers()
                .get("l5d-orig-proto")
                .into_iter()
                .fold(rsp, |rsp, orig_proto| {
                    rsp.header(WAS_ORIG_PROTO, orig_proto)
                });
            future::ok::<_, Infallible>(rsp.body(hyper::Body::default()).unwrap())
        });

        let mut http = hyper::server::conn::Http::new();
        match version {
            ::http::Version::HTTP_10 | ::http::Version::HTTP_11 => http.http1_only(true),
            ::http::Version::HTTP_2 => http.http2_only(true),
            _ => unreachable!("unsupported HTTP version {:?}", version),
        };

        let (client_io, server_io) = io::duplex(4096);
        tokio::spawn(http.serve_connection(server_io, svc));
        Ok(io::BoxedIo::new(client_io))
    }
}

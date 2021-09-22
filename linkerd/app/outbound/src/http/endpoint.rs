use super::{NewRequireIdentity, ProxyConnectionClose};
use crate::Outbound;
use linkerd_app_core::{
    classify, config, errors, http_tracing, metrics,
    proxy::{http, tap},
    svc::{self, ExtractParam},
    tls, Error, Result, CANONICAL_DST_HEADER,
};
use tokio::io;

#[derive(Copy, Clone, Debug)]
struct ClientRescue;

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
        C: svc::Service<T> + Clone + Send + Sync + Unpin + 'static,
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
            connect
                .push(http::client::layer(h1_settings, h2_settings))
                .push_on_service(svc::MapErr::layer(Into::<Error>::into))
                .check_service::<T>()
                .into_new_service()
                .push_new_reconnect(backoff)
                // Tear down server connections when a peer proxy generates an error.
                // TODO(ver) this should only be honored when forwarding and not when the connection
                // is part of a balancer.
                .push(ProxyConnectionClose::layer())
                // Handle connection-level errors eagerly so that we can report 5XX failures in tap
                // and metrics. HTTP error metrics are not incremented here so that errors are not
                // double-counted--i.e., endpoint metrics track these responses and error metrics
                // track proxy errors that occur higher in the stack.
                .push(ClientRescue::layer())
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
    ) -> impl svc::layer::Layer<N, Service = errors::NewRespondService<Self, Self, N>> + Clone {
        errors::respond::layer(Self)
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
        errors::respond::EmitHeaders(true)
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

#[cfg(test)]
mod test {
    use super::*;
    use crate::{http, test_util::*, transport::addrs::*};
    use linkerd_app_core::{
        io,
        proxy::api_resolve::Metadata,
        svc::{NewService, ServiceExt},
        Infallible,
    };
    use std::net::SocketAddr;

    static WAS_ORIG_PROTO: &str = "request-orig-proto";

    /// Tests that the the HTTP endpoint stack forwards connections without HTTP upgrading.
    #[tokio::test(flavor = "current_thread")]
    async fn http11_forward() {
        let _trace = linkerd_tracing::test::trace_init();

        let addr = SocketAddr::new([192, 0, 2, 41].into(), 2041);

        let connect = support::connect()
            .endpoint_fn_boxed(addr, |_: http::Endpoint| serve(::http::Version::HTTP_11));

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
            .endpoint_fn_boxed(addr, |_: http::Endpoint| serve(::http::Version::HTTP_2));

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
            .endpoint_fn_boxed(addr, |_: http::Endpoint| serve(::http::Version::HTTP_2));

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
            metadata: Metadata::new(
                None,
                support::resolver::ProtocolHint::Http2,
                None,
                None,
                None,
            ),
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

    /// Tests that the the HTTP endpoint stack ignores protocol upgrade hinting for HTTP/2 traffic.
    #[tokio::test(flavor = "current_thread")]
    async fn orig_proto_http2_noop() {
        let _trace = linkerd_tracing::test::trace_init();

        let addr = SocketAddr::new([192, 0, 2, 41].into(), 2041);

        // Pretend the upstream is a proxy that supports proto upgrades...
        let connect = support::connect()
            .endpoint_fn_boxed(addr, |_: http::Endpoint| serve(::http::Version::HTTP_2));

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
            metadata: Metadata::new(
                None,
                support::resolver::ProtocolHint::Http2,
                None,
                None,
                None,
            ),
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
    #[allow(clippy::unnecessary_wraps)]
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

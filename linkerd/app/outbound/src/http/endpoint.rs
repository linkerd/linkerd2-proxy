use super::require_identity_on_endpoint::NewRequireIdentity;
use crate::Outbound;
use linkerd_app_core::{
    classify, config, http_tracing, metrics,
    proxy::{http, tap},
    reconnect, svc, tls, Error, CANONICAL_DST_HEADER, L5D_REQUIRE_ID,
};
use tokio::io;

impl<C> Outbound<C> {
    pub fn push_http_endpoint<T, B>(
        self,
    ) -> Outbound<
        impl svc::NewService<
                T,
                Service = impl svc::Service<
                    http::Request<B>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                >,
            > + Clone,
    >
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
        C::Future: Send + Unpin,
    {
        let Self {
            config,
            runtime: rt,
            stack: connect,
        } = self;
        let config::ConnectConfig {
            h1_settings,
            h2_settings,
            backoff,
            ..
        } = config.proxy.connect;

        // Initiates an HTTP client on the underlying transport. Prior-knowledge HTTP/2
        // is typically used (i.e. when communicating with other proxies); though
        // HTTP/1.x fallback is supported as needed.
        let stack = connect
            .push(http::client::layer(h1_settings, h2_settings))
            .check_service::<T>()
            // Re-establishes a connection when the client fails.
            .push(reconnect::layer({
                let backoff = backoff;
                move |_| Ok(backoff.stream())
            }))
            .push(tap::NewTapHttp::layer(rt.tap.clone()))
            .push(rt.metrics.http_endpoint.to_layer::<classify::Response, _>())
            .push_on_response(http_tracing::client(
                rt.span_sink.clone(),
                crate::trace_labels(),
            ))
            .push_on_response(http::strip_header::request::layer(L5D_REQUIRE_ID))
            .push(NewRequireIdentity::layer())
            .push(http::NewOverrideAuthority::layer(vec![
                "host",
                CANONICAL_DST_HEADER,
            ]))
            .push_on_response(http::BoxResponse::layer());

        Outbound {
            config,
            runtime: rt,
            stack,
        }
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
        Never,
    };
    use std::net::SocketAddr;

    /// Tests that the the HTTP endpoint stack forwards connections without HTTP upgrading.
    #[tokio::test(flavor = "current_thread")]
    async fn http11_forward() {
        let _trace = support::trace_init();

        let addr = SocketAddr::new([192, 0, 2, 41].into(), 2041);

        let connect = support::connect().endpoint_fn_boxed(addr, |_: http::Endpoint| {
            let (client_io, server_io) = io::duplex(4096);
            let svc = hyper::service::service_fn(|req: http::Request<_>| {
                tracing::debug!(?req);
                assert_eq!(req.version(), ::http::Version::HTTP_11);
                assert!(
                    req.headers().get("l5d-orig-proto").is_none(),
                    "l5d-orig-proto must not be set"
                );

                let rsp = http::Response::builder()
                    .status(http::StatusCode::NO_CONTENT)
                    .body(hyper::Body::default())
                    .unwrap();
                future::ok::<_, Never>(rsp)
            });

            tokio::spawn(
                hyper::server::conn::Http::new()
                    .http1_only(true)
                    .serve_connection(server_io, svc),
            );

            Ok(io::BoxedIo::new(client_io))
        });

        // Build the outbound server
        let (rt, _shutdown) = runtime();
        let mut stack = Outbound::new(default_config(), rt)
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
            .body(http::BoxBody::default())
            .unwrap();
        let rsp = svc.oneshot(req).await.unwrap();
        assert_eq!(rsp.status(), http::StatusCode::NO_CONTENT);
    }

    /// Tests that the the HTTP endpoint stack forwards connections without HTTP upgrading.
    #[tokio::test(flavor = "current_thread")]
    async fn http2_forward() {
        let _trace = support::trace_init();

        let addr = SocketAddr::new([192, 0, 2, 41].into(), 2042);

        let connect = support::connect().endpoint_fn_boxed(addr, |_: http::Endpoint| {
            let (client_io, server_io) = io::duplex(4096);
            let svc = hyper::service::service_fn(|req: http::Request<_>| {
                tracing::debug!(?req);
                assert_eq!(req.version(), ::http::Version::HTTP_2);
                assert!(
                    req.headers().get("l5d-orig-proto").is_none(),
                    "l5d-orig-proto must not be set"
                );

                let rsp = http::Response::builder()
                    .status(http::StatusCode::NO_CONTENT)
                    .body(hyper::Body::default())
                    .unwrap();
                future::ok::<_, Never>(rsp)
            });

            tokio::spawn(
                hyper::server::conn::Http::new()
                    .http2_only(true)
                    .serve_connection(server_io, svc),
            );

            Ok(io::BoxedIo::new(client_io))
        });

        // Build the outbound server
        let (rt, _shutdown) = runtime();
        let mut stack = Outbound::new(default_config(), rt)
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
            .body(http::BoxBody::default())
            .unwrap();
        let rsp = svc.oneshot(req).await.unwrap();
        assert_eq!(rsp.status(), http::StatusCode::NO_CONTENT);
    }

    /// Tests that the the HTTP endpoint stack uses endpoint metadata to initiate HTTP/2 protocol
    /// upgrading.
    #[tokio::test(flavor = "current_thread")]
    async fn orig_proto_upgrade() {
        let _trace = support::trace_init();

        let addr = SocketAddr::new([192, 0, 2, 41].into(), 2041);

        // Pretend the upstream is a proxy that supports proto upgrades...
        let connect = support::connect().endpoint_fn_boxed(addr, |_: http::Endpoint| {
            let (client_io, server_io) = io::duplex(4096);
            let svc = hyper::service::service_fn(|req: http::Request<_>| {
                tracing::debug!(?req);
                assert_eq!(req.version(), ::http::Version::HTTP_2);
                let orig_proto = req
                    .headers()
                    .get("l5d-orig-proto")
                    .expect("l5d-orig-proto must be set");
                assert_eq!(orig_proto, "HTTP/1.1");

                let rsp = http::Response::builder()
                    .status(http::StatusCode::NO_CONTENT)
                    .body(hyper::Body::default())
                    .unwrap();
                future::ok::<_, Never>(rsp)
            });

            tokio::spawn(
                hyper::server::conn::Http::new()
                    .http2_only(true)
                    .serve_connection(server_io, svc),
            );

            Ok(io::BoxedIo::new(client_io))
        });

        // Build the outbound server
        let (rt, _shutdown) = runtime();
        let mut stack = Outbound::new(default_config(), rt)
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
                Default::default(),
                support::resolver::ProtocolHint::Http2,
                None,
                None,
                None,
            ),
        });

        let req = http::Request::builder()
            .uri("http://foo.example.com")
            .body(http::BoxBody::default())
            .unwrap();
        let rsp = svc.oneshot(req).await.unwrap();
        assert_eq!(rsp.status(), http::StatusCode::NO_CONTENT);
    }
}

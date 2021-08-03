use http::{Request, Response};
use linkerd_app_core::{
    errors::L5D_PROXY_ERROR,
    svc::{layer, NewService, Service},
};
use linkerd_proxy_http::ClientHandle;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// Close the accepted connection if the response from a peer proxy has the
/// l5d-proxy-error header. This means the peer proxy encountered an inbound
/// connection error with its application and therefore the accepted
/// connection should be torn down.
#[derive(Clone, Debug)]
pub struct PeerProxyErrors<N> {
    inner: N,
}

impl<N> PeerProxyErrors<N> {
    fn new(inner: N) -> Self {
        Self { inner }
    }

    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone + Copy {
        layer::mk(Self::new)
    }
}

impl<T, N> NewService<T> for PeerProxyErrors<N>
where
    N: NewService<T>,
{
    type Service = PeerProxyErrors<N::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        let inner = self.inner.new_service(target);
        PeerProxyErrors { inner }
    }
}

impl<S, A, B> Service<Request<A>> for PeerProxyErrors<S>
where
    S: Service<Request<A>, Response = Response<B>>,
    S::Response: Send,
    S::Future: Send + 'static,
    A: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<A>) -> Self::Future {
        let client = req.extensions().get::<ClientHandle>().cloned();
        debug_assert!(client.is_some(), "Missing client handle");
        let response = self.inner.call(req);
        Box::pin(async move {
            let response = response.await?;
            if let Some(message) = response.headers().get(L5D_PROXY_ERROR) {
                tracing::info!(?message, "response contained `{}` header", L5D_PROXY_ERROR);
                // Gracefully teardown the accepted connection.
                if let Some(ClientHandle { close, .. }) = client {
                    tracing::info!("connection closed");
                    close.close();
                }
            }
            Ok(response)
        })
    }
}

#[cfg(test)]
mod test {
    use crate::{
        http::{Endpoint, NewServeHttp, Request, Response, StatusCode, Version},
        test_util::{
            self, future,
            support::{self, connect::Connect, http_util},
        },
        transport::addrs::{Remote, ServerAddr},
        Config, Outbound,
    };
    use hyper::{client::conn::Builder, server::conn::Http, service, Body};
    use linkerd_app_core::{
        errors::L5D_PROXY_ERROR,
        io,
        proxy::api_resolve::Metadata,
        svc::NewService,
        svc::{BoxNewService, BoxService},
        tls::{ConditionalClientTls, NoClientTls},
        Error, Infallible, ProxyRuntime,
    };
    use linkerd_proxy_http::BoxRequest;
    use linkerd_tracing::test;
    use std::net::SocketAddr;

    #[tokio::test(flavor = "current_thread")]
    async fn connection_closes_after_response_header() {
        let _trace = test::trace_init();

        // Build the outbound server that responds with the l5d-proxy-error
        // header set.
        let (rt, _shutdown) = test_util::runtime();
        let addr = SocketAddr::new([127, 0, 0, 1].into(), 12345);
        let connect = support::connect().endpoint_fn_boxed(addr, |_: Endpoint| serve());
        let config = test_util::default_config();
        let mut outbound = build_outbound(config, rt, connect);

        // Build the otubound service.
        let service = outbound.new_service(Endpoint {
            addr: Remote(ServerAddr(addr)),
            tls: ConditionalClientTls::None(NoClientTls::Disabled),
            metadata: Metadata::default(),
            logical_addr: None,
            protocol: Version::Http1,
            opaque_protocol: false,
        });

        // Build the client that should be closed after receiving a response
        // with the l5d-proxy-error header.
        let mut builder = Builder::new();
        let (mut client, bg) = http_util::connect_and_accept(&mut builder, service).await;

        let mut request = Request::builder()
            .uri("http://foo.example.com")
            .body(Body::default())
            .unwrap();
        request = test_util::add_client_handle(request, addr);
        let response = http_util::http_request(&mut client, request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let message = response
            .headers()
            .get(L5D_PROXY_ERROR)
            .expect("response did not contain l5d-proxy-error header");
        assert_eq!(message, "proxy received invalid response");

        // The client's background task should be completed without dropping
        // the client because the connection was closed after encountering a
        // response that contains the l5d-proxy-error header.
        let _ = bg.await;
    }

    fn build_outbound<I>(
        config: Config,
        rt: ProxyRuntime,
        connect: Connect<Endpoint>,
    ) -> BoxNewService<Endpoint, BoxService<I, (), Error>>
    where
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Unpin + 'static,
    {
        Outbound::new(config.clone(), rt.clone())
            .with_stack(connect)
            .push_http_endpoint()
            .push_http_server()
            .map_stack(|_, _, s| s.push_on_response(BoxRequest::layer()))
            .push(NewServeHttp::layer(
                config.proxy.server.h2_settings,
                rt.drain,
            ))
            .map_stack(|_, _, s| s.push_on_response(BoxService::layer()))
            .push(BoxNewService::layer())
            .into_inner()
    }

    fn serve() -> io::Result<io::BoxedIo> {
        let (client_io, server_io) = io::duplex(4096);
        let http = Http::new();
        let service = service::service_fn(move |_: Request<Body>| {
            let response = Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .header(L5D_PROXY_ERROR, "proxy received invalid response")
                .body(hyper::Body::default())
                .unwrap();
            future::ok::<_, Infallible>(response)
        });
        tokio::spawn(http.serve_connection(server_io, service));
        Ok(io::BoxedIo::new(client_io))
    }
}

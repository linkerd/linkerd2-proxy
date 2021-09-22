use futures::prelude::*;
use linkerd_app_core::{
    errors::respond::{L5D_PROXY_CONNECTION, L5D_PROXY_ERROR},
    proxy::http::ClientHandle,
    svc, tls,
};
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
pub struct ProxyConnectionClose<N> {
    inner: N,
}

#[pin_project::pin_project]
#[derive(Debug)]
pub struct ResponseFuture<F> {
    #[pin]
    inner: F,
    client: ClientHandle,
}

impl<N> ProxyConnectionClose<N> {
    fn new(inner: N) -> Self {
        Self { inner }
    }

    pub fn layer() -> impl svc::layer::Layer<N, Service = Self> + Clone + Copy {
        svc::layer::mk(Self::new)
    }
}

impl<T, N> svc::NewService<T> for ProxyConnectionClose<N>
where
    N: svc::NewService<T>,
{
    type Service = ProxyConnectionClose<N::Service>;

    #[inline]
    fn new_service(&self, target: T) -> Self::Service {
        let inner = self.inner.new_service(target);
        ProxyConnectionClose { inner }
    }
}

impl<S, A, B> svc::Service<http::Request<A>> for ProxyConnectionClose<S>
where
    S: svc::Service<http::Request<A>, Response = http::Response<B>>,
    S::Response: Send,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        let client = req
            .extensions()
            .get::<ClientHandle>()
            .cloned()
            .expect("missing client handle");
        let inner = self.inner.call(req);
        ResponseFuture { inner, client }
    }
}

impl<B, F: TryFuture<Ok = http::Response<B>>> Future for ResponseFuture<F> {
    type Output = Result<http::Response<B>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let mut rsp = futures::ready!(this.inner.try_poll(cx))?;

        // Clear the header...
        if let Some(proxy_conn) = rsp.headers_mut().remove(L5D_PROXY_CONNECTION) {
            if proxy_conn == "close" {
                match rsp.extensions().get::<tls::ConditionalClientTls>() {
                    Some(tls::ConditionalClientTls::Some(_)) => {
                        if let Some(error) = rsp
                            .headers()
                            .get(L5D_PROXY_ERROR)
                            .and_then(|v| v.to_str().ok())
                        {
                            tracing::info!(%error, "Closing application connection for remote proxy");
                        } else {
                            tracing::info!("Closing application connection for remote proxy");
                        }

                        if rsp.version() == http::Version::HTTP_11 {
                            // If the response is HTTP/1.1, we need to send a Connection: close
                            // header to tell the application this connection is being closed.
                            rsp.headers_mut().insert(
                                http::header::CONNECTION,
                                http::HeaderValue::from_static("close"),
                            );
                        }

                        // Signal that the proxy's server-side connection should be terminated. This handles
                        // the remote error as if the local proxy encountered an error.
                        this.client.close.close();
                    }
                    _ => {
                        tracing::info!(
                            "Received unmeshed response with {} set",
                            L5D_PROXY_CONNECTION
                        );
                    }
                }
            }
        }

        Poll::Ready(Ok(rsp))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use futures::future;
    use linkerd_app_core::{
        svc::{self, ServiceExt},
        Infallible,
    };
    use linkerd_tracing::test;
    use tokio::time;

    #[tokio::test(flavor = "current_thread")]
    async fn connection_closes_after_meshed_response_header() {
        let _trace = test::trace_init();

        // Build the client that should be closed after receiving a response
        // with the l5d-proxy-error header.
        let mut req = http::Request::builder()
            .uri("http://foo.example.com")
            .body(hyper::Body::default())
            .unwrap();
        let (handle, closed) = ClientHandle::new(([192, 0, 2, 3], 50000).into());
        req.extensions_mut().insert(handle);

        let svc = ProxyConnectionClose::new(svc::mk(move |_: http::Request<hyper::Body>| {
            future::ok::<_, Infallible>(
                http::Response::builder()
                    .status(http::StatusCode::BAD_GATEWAY)
                    .header(L5D_PROXY_CONNECTION, "close")
                    .extension(tls::ConditionalClientTls::Some(tls::ClientTls {
                        server_id: "foosa.barns.serviceaccount.identity.linkerd.cluster.local"
                            .parse()
                            .unwrap(),
                        alpn: None,
                    }))
                    .body(hyper::Body::default())
                    .unwrap(),
            )
        }));

        let rsp = svc.oneshot(req).await.expect("request must succeed");
        assert_eq!(rsp.status(), http::StatusCode::BAD_GATEWAY);

        // The client handle close future should fire.
        time::timeout(time::Duration::from_secs(10), closed)
            .await
            .expect("client handle must close");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn header_ignored_in_unmeshed_response_header() {
        let _trace = test::trace_init();

        // Build the client that should be closed after receiving a response
        // with the l5d-proxy-error header.
        let mut req = http::Request::builder()
            .uri("http://foo.example.com")
            .body(hyper::Body::default())
            .unwrap();
        let (handle, closed) = ClientHandle::new(([192, 0, 2, 3], 50000).into());
        req.extensions_mut().insert(handle);

        let svc = ProxyConnectionClose::new(svc::mk(move |_: http::Request<hyper::Body>| {
            future::ok::<_, Infallible>(
                http::Response::builder()
                    .status(http::StatusCode::BAD_GATEWAY)
                    .header(L5D_PROXY_CONNECTION, "close")
                    .body(hyper::Body::default())
                    .unwrap(),
            )
        }));

        let rsp = svc.oneshot(req).await.expect("request must succeed");
        assert_eq!(rsp.status(), http::StatusCode::BAD_GATEWAY);

        // The client handle close future should fire.
        tokio::select! {
            _ = time::sleep(time::Duration::from_secs(10)) => {},
            _ = closed => panic!("connection shouldn't close"),
        }
    }
}

use http::{Request, Response};
use linkerd_app_core::{
    errors::L5D_PROXY_ERROR,
    proxy::http::ClientHandle,
    svc::{layer, NewService, Service},
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
                    tracing::trace!("connection closed");
                    close.close();
                }
            }
            Ok(response)
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test_util;
    use futures::future;
    use linkerd_app_core::{
        svc::{self, ServiceExt},
        Infallible,
    };
    use linkerd_tracing::test;

    #[tokio::test(flavor = "current_thread")]
    async fn connection_closes_after_response_header() {
        let _trace = test::trace_init();

        let service = PeerProxyErrors::new(svc::mk(move |_: http::Request<hyper::Body>| {
            let response = http::Response::builder()
                .status(http::StatusCode::BAD_GATEWAY)
                .header(L5D_PROXY_ERROR, "proxy received invalid response")
                .body(hyper::Body::default())
                .unwrap();
            future::ok::<_, Infallible>(response)
        }));

        // Build the client that should be closed after receiving a response
        // with the l5d-proxy-error header.
        let mut request = http::Request::builder()
            .uri("http://foo.example.com")
            .body(hyper::Body::default())
            .unwrap();
        let closed = test_util::set_client_handle(&mut request, ([192, 0, 2, 3], 50000).into());
        let response = service
            .oneshot(request)
            .await
            .expect("request must succeed");
        assert_eq!(response.status(), http::StatusCode::BAD_GATEWAY);
        let message = response
            .headers()
            .get(L5D_PROXY_ERROR)
            .expect("response did not contain l5d-proxy-error header");
        assert_eq!(message, "proxy received invalid response");

        // The client handle close future should fire.
        tokio::time::timeout(tokio::time::Duration::from_secs(10), closed)
            .await
            .expect("client handle must close");
    }
}

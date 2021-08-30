use futures::prelude::*;
use linkerd_app_core::{errors::respond::L5D_PROXY_ERROR, proxy::http::ClientHandle, svc};
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

    pub fn layer() -> impl svc::layer::Layer<N, Service = Self> + Clone + Copy {
        svc::layer::mk(Self::new)
    }
}

impl<T, N> svc::NewService<T> for PeerProxyErrors<N>
where
    N: svc::NewService<T>,
{
    type Service = PeerProxyErrors<N::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        let inner = self.inner.new_service(target);
        PeerProxyErrors { inner }
    }
}

impl<S, A, B> svc::Service<http::Request<A>> for PeerProxyErrors<S>
where
    S: svc::Service<http::Request<A>, Response = http::Response<B>>,
    S::Response: Send,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        let ClientHandle { close, .. } = req
            .extensions()
            .get::<ClientHandle>()
            .cloned()
            .expect("missing client handle");

        Box::pin(self.inner.call(req).map_ok(move |rsp| {
            if let Some(msg) = rsp.headers().get(L5D_PROXY_ERROR) {
                tracing::debug!(
                    ?msg,
                    "Received an error response from a peer proxy; closing connection"
                );

                // Signal that the proxy's server-side connection should be terminated. This handles
                // the remote error as if the local proxy encountered an error.
                close.close();
            }

            rsp
        }))
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
    async fn connection_closes_after_response_header() {
        let _trace = test::trace_init();

        // Build the client that should be closed after receiving a response
        // with the l5d-proxy-error header.
        let mut req = http::Request::builder()
            .uri("http://foo.example.com")
            .body(hyper::Body::default())
            .unwrap();
        let (handle, closed) = ClientHandle::new(([192, 0, 2, 3], 50000).into());
        req.extensions_mut().insert(handle);

        const ERROR_MSG: &str = "something bad happened";
        let svc = PeerProxyErrors::new(svc::mk(move |_: http::Request<hyper::Body>| {
            future::ok::<_, Infallible>(
                http::Response::builder()
                    .status(http::StatusCode::BAD_GATEWAY)
                    .header(L5D_PROXY_ERROR, ERROR_MSG)
                    .body(hyper::Body::default())
                    .unwrap(),
            )
        }));

        let rsp = svc.oneshot(req).await.expect("request must succeed");
        assert_eq!(rsp.status(), http::StatusCode::BAD_GATEWAY);
        assert_eq!(
            rsp.headers()
                .get(L5D_PROXY_ERROR)
                .expect("response did not contain l5d-proxy-error header"),
            ERROR_MSG
        );

        // The client handle close future should fire.
        time::timeout(time::Duration::from_secs(10), closed)
            .await
            .expect("client handle must close");
    }
}

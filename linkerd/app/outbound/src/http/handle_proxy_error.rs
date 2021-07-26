use http::{Request, Response};
use linkerd_app_core::{
    svc::{layer, NewService, Service},
    Error,
};
use linkerd_proxy_http::ClientHandle;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Clone, Debug)]
pub struct NewHandleProxyError<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub struct HandleProxyError<N> {
    inner: N,
}

impl<N> NewHandleProxyError<N> {
    fn new(inner: N) -> Self {
        Self { inner }
    }

    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone + Copy {
        layer::mk(Self::new)
    }
}

impl<T, N> NewService<T> for NewHandleProxyError<N>
where
    N: NewService<T>,
{
    type Service = HandleProxyError<N::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        let inner = self.inner.new_service(target);
        HandleProxyError { inner }
    }
}

impl<S, A, B> Service<Request<A>> for HandleProxyError<S>
where
    S: Service<Request<A>, Response = Response<B>>,
    S::Response: Send,
    S::Error: Into<Error> + Send,
    S::Future: Send + 'static,
    A: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: Request<A>) -> Self::Future {
        let client = req.extensions().get::<ClientHandle>().cloned();
        debug_assert!(client.is_some(), "Missing client handle");
        let response = self.inner.call(req);
        Box::pin(async move {
            match response.await {
                Ok(rsp) => {
                    if let Some(_) = rsp.headers().get(linkerd_app_core::errors::L5D_PROXY_ERROR) {
                        // Gracefully teardown the accepted connection.
                        if let Some(ClientHandle { close, .. }) = client {
                            close.close();
                        }
                    }
                    Ok(rsp)
                }
                Err(e) => Err(e.into()),
            }
        })
    }
}

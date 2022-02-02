use linkerd_error::{is_error, Error};
use parking_lot::RwLock;
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

#[derive(Clone, Debug)]
pub struct FailOnError<E, S> {
    inner: S,
    error: Arc<RwLock<Option<SharedError>>>,
    _marker: std::marker::PhantomData<E>,
}

#[derive(Clone, Debug)]
struct SharedError(Arc<Error>);

impl<E, S> FailOnError<E, S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            error: Arc::new(RwLock::new(None)),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<E, S, Req> tower::Service<Req> for FailOnError<E, S>
where
    S: tower::Service<Req>,
    S::Response: Send,
    S::Error: Into<Error> + Send,
    S::Future: Send + 'static,
    E: std::error::Error + 'static,
{
    type Response = S::Response;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        futures::ready!(self.inner.poll_ready(cx).map_err(Into::into))?;
        if let Some(e) = &*self.error.read() {
            return Poll::Ready(Err(e.clone().into()));
        }
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let fut = self.inner.call(req);
        let error = self.error.clone();
        Box::pin(async move {
            match fut.await {
                Ok(rsp) => Ok(rsp),
                Err(e) => {
                    let e = e.into();
                    if is_error::<E>(&*e) {
                        let e = SharedError(Arc::new(e));
                        *error.write() = Some(e.clone());
                        Err(e.into())
                    } else {
                        Err(e)
                    }
                }
            }
        })
    }
}

impl std::fmt::Display for SharedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for SharedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&**self.0.as_ref())
    }
}

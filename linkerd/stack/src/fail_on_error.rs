use linkerd2_error::Error;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, RwLock},
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
        if let Ok(e) = self.error.read() {
            if let Some(e) = &*e {
                return Poll::Ready(Err(e.clone().into()));
            }
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
                        if let Ok(mut error) = error.write() {
                            *error = Some(e.clone());
                        }
                        Err(e.into())
                    } else {
                        Err(e)
                    }
                }
            }
        })
    }
}

fn is_error<E: std::error::Error + 'static>(e: &(dyn std::error::Error + 'static)) -> bool {
    e.is::<E>() || e.source().map(is_error::<E>).unwrap_or(false)
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

use futures::{future, prelude::*};
use linkerd_error::Error;
use std::{
    fmt,
    sync::Arc,
    task::{Context, Poll},
};

#[derive(Clone, Debug)]
pub struct ResultService<S>(Inner<S>);

#[derive(Clone, Debug)]
struct SharedError(Arc<Error>);

#[derive(Clone, Debug)]
enum Inner<S> {
    Ok(S),
    Err(SharedError),
}

impl<S> ResultService<S> {
    pub fn ok(svc: S) -> Self {
        ResultService(Inner::Ok(svc))
    }

    pub fn err<E: Into<Error>>(err: E) -> Self {
        ResultService(Inner::Err(SharedError(Arc::new(err.into()))))
    }
}

impl<S, E: Into<Error>> From<Result<S, E>> for ResultService<S> {
    fn from(res: Result<S, E>) -> Self {
        match res {
            Ok(s) => Self::ok(s),
            Err(e) => Self::err(e),
        }
    }
}

impl<T, S> tower::Service<T> for ResultService<S>
where
    S: tower::Service<T>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::ErrInto<S::Future, Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        match self.0 {
            Inner::Ok(ref mut svc) => svc.poll_ready(cx).map_err(Into::into),
            Inner::Err(ref err) => Poll::Ready(Err(err.clone().into())),
        }
    }

    fn call(&mut self, t: T) -> Self::Future {
        match self.0 {
            Inner::Ok(ref mut svc) => svc.call(t).err_into::<Error>(),
            Inner::Err(_) => panic!("poll_ready not called"),
        }
    }
}

impl fmt::Display for SharedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for SharedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&**self.0)
    }
}

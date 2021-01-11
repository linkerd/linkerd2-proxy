use futures::{future, prelude::*, ready};
use linkerd_error::Error;
use std::task::{Context, Poll};

#[derive(Copy, Clone, Debug)]
pub struct ResultService<S, E>(Inner<S, E>);

#[derive(Copy, Clone, Debug)]
enum Inner<S, E> {
    Ok(S),
    Err(Option<E>),
}

impl<S, E> ResultService<S, E> {
    pub fn ok(svc: S) -> Self {
        ResultService(Inner::Ok(svc))
    }

    pub fn err(err: E) -> Self {
        ResultService(Inner::Err(Some(err)))
    }
}

impl<S, E> From<Result<S, E>> for ResultService<S, E> {
    fn from(res: Result<S, E>) -> Self {
        match res {
            Ok(s) => Self::ok(s),
            Err(e) => Self::err(e),
        }
    }
}

impl<T, S, E> tower::Service<T> for ResultService<S, E>
where
    S: tower::Service<T>,
    S::Error: Into<Error>,
    E: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::ErrInto<S::Future, Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let res = match self.0 {
            Inner::Ok(ref mut svc) => ready!(svc.poll_ready(cx)).map_err(Into::into),
            Inner::Err(ref mut err) => Err(err.take().expect("polled after failure").into()),
        };
        Poll::Ready(res)
    }

    fn call(&mut self, t: T) -> Self::Future {
        match self.0 {
            Inner::Ok(ref mut svc) => svc.call(t).err_into::<Error>(),
            Inner::Err(_) => panic!("poll_ready not called"),
        }
    }
}

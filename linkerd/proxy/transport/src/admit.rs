use futures::{Future, Poll};
use linkerd2_error::Error;

pub struct AdmitLayer<A>(A);

pub trait Admit<T> {
    type Error: Into<Error>;
    fn admit(&mut self, target: &T) -> Result<(), Self::Error>;
}

#[derive(Debug, Clone)]
pub struct AdmitService<A, S> {
    admit: A,
    inner: S,
}

pub enum AdmitFuture<F, E> {
    Admit(F),
    Failed(Option<E>),
}

impl<A> AdmitLayer<A> {
    pub fn new(admit: A) -> Self {
        Self(admit)
    }
}

impl<A: Clone, S> tower::layer::Layer<S> for AdmitLayer<A> {
    type Service = AdmitService<A, S>;

    fn layer(&self, inner: S) -> Self::Service {
        Self::Service {
            admit: self.0.clone(),
            inner: inner,
        }
    }
}

impl<A, T, S> tower::Service<T> for AdmitService<A, S>
where
    A: Admit<T>,
    S: tower::Service<T>,
    S::Error: Into<Error> + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = Error;
    type Future = AdmitFuture<S::Future, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, t: T) -> Self::Future {
        match self.admit.admit(&t) {
            Ok(()) => AdmitFuture::Admit(self.inner.call(t)),
            Err(e) => AdmitFuture::Failed(Some(e.into())),
        }
    }
}

impl<F, E> Future for AdmitFuture<F, E>
where
    F: Future,
    F::Error: Into<Error>,
    E: Into<Error>,
{
    type Item = F::Item;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self {
            //AdmitFuture::Failed()(ref mut f) => f.poll().map_err(Into::into),
            AdmitFuture::Admit(ref mut f) => f.poll().map_err(Into::into),
            AdmitFuture::Failed(e) => Err(e.take().unwrap().into()),
        }
    }
}

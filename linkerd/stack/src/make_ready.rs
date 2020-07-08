use futures::{ready, TryFuture};
use linkerd2_error::Error;
use pin_project::pin_project;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

#[derive(Debug)]
pub struct MakeReadyLayer<Req>(PhantomData<fn(Req)>);

#[derive(Debug)]
pub struct MakeReady<M, Req>(M, PhantomData<fn(Req)>);

#[pin_project]
#[derive(Debug)]
pub struct MakeReadyFuture<F, S, Req> {
    #[pin]
    state: State<F, S>,
    _req: PhantomData<fn(Req)>,
}

#[pin_project(project = StateProj)]
#[derive(Debug)]
enum State<F, S> {
    Making(#[pin] F),
    Ready(Option<S>),
}

impl<Req> MakeReadyLayer<Req> {
    pub fn new() -> Self {
        MakeReadyLayer(PhantomData)
    }
}

impl<M, Req> tower::layer::Layer<M> for MakeReadyLayer<Req> {
    type Service = MakeReady<M, Req>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeReady(inner, self.0)
    }
}

impl<Req> Clone for MakeReadyLayer<Req> {
    fn clone(&self) -> Self {
        MakeReadyLayer(self.0)
    }
}

impl<T, M, S, Req> tower::Service<T> for MakeReady<M, Req>
where
    M: tower::Service<T, Response = S>,
    M::Error: Into<Error>,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S;
    type Error = Error;
    type Future = MakeReadyFuture<M::Future, S, Req>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, t: T) -> Self::Future {
        MakeReadyFuture {
            state: State::Making(self.0.call(t)),
            _req: PhantomData,
        }
    }
}

impl<M: Clone, Req> Clone for MakeReady<M, Req> {
    fn clone(&self) -> Self {
        MakeReady(self.0.clone(), self.1)
    }
}

impl<F, S, Req> Future for MakeReadyFuture<F, S, Req>
where
    F: TryFuture<Ok = S>,
    F::Error: Into<Error>,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Output = Result<S, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        loop {
            match this.state.as_mut().project() {
                StateProj::Making(fut) => {
                    let svc = ready!(fut.try_poll(cx)).map_err(Into::into)?;
                    this.state.set(State::Ready(Some(svc)));
                }
                StateProj::Ready(svc) => {
                    let _ = ready!(svc.as_mut().expect("polled after ready!").poll_ready(cx))
                        .map_err(Into::into)?;
                    return Poll::Ready(Ok(svc.take().expect("polled after ready!")));
                }
            }
        }
    }
}

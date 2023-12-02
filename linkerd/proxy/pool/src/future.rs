use super::message;
use futures::ready;
use linkerd_error::Error;
use pin_project::pin_project;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[pin_project]
/// Future that completes when the buffered service eventually services the submitted request.
#[derive(Debug)]
pub struct ResponseFuture<T> {
    #[pin]
    state: ResponseState<T>,
}

#[pin_project(project = ResponseStateProj)]
#[derive(Debug)]
enum ResponseState<T> {
    Failed {
        error: Option<Error>,
    },
    Rx {
        #[pin]
        rx: message::Rx<T>,
    },
    Poll {
        #[pin]
        fut: T,
    },
}

impl<T> ResponseFuture<T> {
    pub(crate) fn new(rx: message::Rx<T>) -> Self {
        ResponseFuture {
            state: ResponseState::Rx { rx },
        }
    }

    pub(crate) fn failed(err: Error) -> Self {
        ResponseFuture {
            state: ResponseState::Failed { error: Some(err) },
        }
    }
}

impl<F, T, E> Future for ResponseFuture<F>
where
    F: Future<Output = Result<T, E>>,
    E: Into<Error>,
{
    type Output = Result<T, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        loop {
            match this.state.as_mut().project() {
                ResponseStateProj::Failed { error } => {
                    return Poll::Ready(Err(error.take().expect("polled after error")));
                }
                ResponseStateProj::Rx { rx } => {
                    let fut = ready!(rx.poll(cx))
                        .expect("worker must set a failure if it exits prematurely")?;
                    this.state.set(ResponseState::Poll { fut });
                }
                ResponseStateProj::Poll { fut } => return fut.poll(cx).map_err(Into::into),
            }
        }
    }
}

use futures::{future, TryFuture};
use linkerd2_duplex::Duplex;
use linkerd2_error::Error;
use pin_project::{pin_project, project};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite};
use tower::Service;

pub fn forward<C>(connect: C) -> Forward<C> {
    Forward { connect }
}

#[derive(Clone, Debug)]
pub struct Forward<C> {
    connect: C,
}

#[pin_project]
pub struct ForwardFuture<I, F: TryFuture> {
    #[pin]
    state: ForwardState<I, F>,
}

#[pin_project]
enum ForwardState<I, F: TryFuture> {
    Connect {
        #[pin]
        connect: F,
        io: Option<I>,
    },
    Duplex(#[pin] Duplex<I, F::Ok>),
}

impl<C> Forward<C> {
    pub fn new(connect: C) -> Self {
        Self { connect }
    }
}

impl<C, T, I> Service<(T, I)> for Forward<C>
where
    C: Service<T>,
    C::Response: Send + 'static,
    C::Future: Send + 'static,
    C::Error: Into<Error>,
    C::Response: AsyncRead + AsyncWrite + Unpin,
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Response = ForwardFuture<I, C::Future>;
    type Error = Error;
    type Future = future::Ready<Result<Self::Response, Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), self::Error>> {
        self.connect.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, (meta, io): (T, I)) -> Self::Future {
        future::ok(ForwardFuture {
            state: ForwardState::Connect {
                io: Some(io),
                connect: self.connect.call(meta),
            },
        })
    }
}

impl<I, F> Future for ForwardFuture<I, F>
where
    I: AsyncRead + AsyncWrite + Unpin,
    F: TryFuture,
    F::Ok: AsyncRead + AsyncWrite + Unpin,
    F::Error: Into<Error>,
{
    type Output = Result<(), Error>;

    #[project]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        loop {
            #[project]
            match this.state.as_mut().project() {
                ForwardState::Connect { connect, io } => {
                    let client_io = futures::ready!(connect.try_poll(cx).map_err(Into::into))?;
                    let server_io = io.take().expect("illegal state");
                    let duplex = Duplex::new(server_io, client_io);
                    this.state.set(ForwardState::Duplex(duplex))
                }
                ForwardState::Duplex(fut) => {
                    return fut.poll(cx).map_err(Into::into);
                }
            }
        }
    }
}

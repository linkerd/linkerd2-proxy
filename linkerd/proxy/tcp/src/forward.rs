use futures::{try_ready, Future, Poll};
use linkerd2_duplex::Duplex;
use linkerd2_error::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tower::Service;

pub fn forward<C>(connect: C) -> Forward<C> {
    Forward { connect }
}

#[derive(Clone, Debug)]
pub struct Forward<C> {
    connect: C,
}

pub enum ForwardFuture<I, F: Future> {
    Connect { connect: F, io: Option<I> },
    Duplex(Duplex<I, F::Item>),
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
    C::Response: AsyncRead + AsyncWrite,
    I: AsyncRead + AsyncWrite + Send + 'static,
{
    type Response = Box<dyn Future<Item = (), Error = Error> + Send + 'static>;
    type Error = Error;
    type Future = Box<dyn Future<Item = Self::Response, Error = Error> + Send + 'static>;

    fn poll_ready(&mut self) -> Poll<(), self::Error> {
        self.connect.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, (meta, io): (T, I)) -> Self::Future {
        let forward_future = Box::new(ForwardFuture::Connect {
            io: Some(io),
            connect: self.connect.call(meta),
        });

        Box::new(futures::future::ok::<Self::Response, Error>(forward_future))
    }
}

impl<I, F> Future for ForwardFuture<I, F>
where
    I: AsyncRead + AsyncWrite,
    F: Future,
    F::Item: AsyncRead + AsyncWrite,
    F::Error: Into<Error>,
{
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<(), Self::Error> {
        loop {
            *self = match self {
                ForwardFuture::Connect {
                    ref mut connect,
                    ref mut io,
                } => {
                    let client_io = try_ready!(connect.poll().map_err(Into::into));
                    let server_io = io.take().expect("illegal state");
                    ForwardFuture::Duplex(Duplex::new(server_io, client_io))
                }
                ForwardFuture::Duplex(ref mut fut) => {
                    return fut.poll().map_err(Into::into);
                }
            }
        }
    }
}

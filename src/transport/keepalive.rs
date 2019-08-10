use std::io;
use std::time::Duration;
use tokio::net::TcpStream;

pub trait SetKeepalive {
    fn keepalive(&self) -> io::Result<Option<Duration>>;
    fn set_keepalive(&mut self, ka: Option<Duration>) -> ::std::io::Result<()>;
}

impl SetKeepalive for TcpStream {
    fn keepalive(&self) -> io::Result<Option<Duration>> {
        TcpStream::keepalive(self)
    }

    fn set_keepalive(&mut self, ka: Option<Duration>) -> ::std::io::Result<()> {
        TcpStream::set_keepalive(self, ka)
    }
}

pub mod accept {
    use std::time::Duration;
    use tokio::io::{AsyncRead, AsyncWrite};
    use tracing::debug;

    use super::SetKeepalive;

    pub fn layer(keepalive: Option<Duration>) -> Accept {
        Accept { keepalive }
    }

    #[derive(Clone, Debug)]
    pub struct Accept {
        keepalive: Option<Duration>,
    }

    impl<I> crate::proxy::Accept<I> for Accept
    where
        I: AsyncRead + AsyncWrite + SetKeepalive,
    {
        type Io = I;

        fn accept(&self, _: &crate::proxy::Source, mut io: I) -> Self::Io {
            if let Err(e) = io.set_keepalive(self.keepalive) {
                debug!("failed to set keepalive: {}", e);
            }

            io
        }
    }
}

pub mod connect {
    use super::SetKeepalive;
    use crate::svc;
    use futures::{try_ready, Future, Poll};
    use std::time::Duration;
    use tracing::debug;

    pub fn layer(keepalive: Option<Duration>) -> Layer {
        Layer { keepalive }
    }

    #[derive(Clone, Debug)]
    pub struct Layer {
        keepalive: Option<Duration>,
    }

    #[derive(Clone, Debug)]
    pub struct Connect<T> {
        keepalive: Option<Duration>,
        inner: T,
    }

    impl<C> svc::Layer<C> for Layer {
        type Service = Connect<C>;

        fn layer(&self, inner: C) -> Self::Service {
            Connect {
                inner,
                keepalive: self.keepalive,
            }
        }
    }

    /// impl MakeConnection
    impl<C, T> svc::Service<T> for Connect<C>
    where
        C: svc::MakeConnection<T>,
        C::Connection: SetKeepalive,
    {
        type Response = C::Connection;
        type Error = C::Error;
        type Future = Connect<C::Future>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.inner.poll_ready()
        }

        fn call(&mut self, target: T) -> Self::Future {
            let keepalive = self.keepalive;
            let inner = self.inner.make_connection(target);
            Connect { keepalive, inner }
        }
    }

    impl<F> Future for Connect<F>
    where
        F: Future,
        F::Item: SetKeepalive,
    {
        type Item = F::Item;
        type Error = F::Error;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            let mut io = try_ready!(self.inner.poll());

            if let Err(e) = io.set_keepalive(self.keepalive) {
                debug!("failed to set keepalive: {}", e);
            }

            Ok(io.into())
        }
    }
}

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

    use super::SetKeepalive;
    use svc;

    pub fn layer(keepalive: Option<Duration>) -> Layer {
        Layer { keepalive }
    }

    #[derive(Clone, Debug)]
    pub struct Layer {
        keepalive: Option<Duration>,
    }

    #[derive(Clone, Debug)]
    pub struct Stack<M> {
        keepalive: Option<Duration>,
        inner: M,
    }

    #[derive(Clone, Debug)]
    pub struct Accept<T> {
        keepalive: Option<Duration>,
        inner: T,
    }

    impl<T, M> svc::Layer<T, T, M> for Layer
    where
        M: svc::Stack<T>,
    {
        type Value = <Stack<M> as svc::Stack<T>>::Value;
        type Error = <Stack<M> as svc::Stack<T>>::Error;
        type Stack = Stack<M>;

        fn bind(&self, inner: M) -> Self::Stack {
            Stack {
                inner,
                keepalive: self.keepalive,
            }
        }
    }

    impl<T, M> svc::Stack<T> for Stack<M>
    where
        M: svc::Stack<T>,
    {
        type Value = Accept<M::Value>;
        type Error = M::Error;

        fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
            let inner = self.inner.make(target)?;
            Ok(Accept {
                inner,
                keepalive: self.keepalive,
            })
        }
    }

    impl<I, A> ::proxy::Accept<I> for Accept<A>
    where
        I: AsyncRead + AsyncWrite + SetKeepalive,
        A: ::proxy::Accept<I>,
    {
        type Io = A::Io;

        fn accept(&self, mut io: I) -> Self::Io {
            if let Err(e) = io.set_keepalive(self.keepalive) {
                debug!("failed to set keepalive: {}", e);
            }

            self.inner.accept(io)
        }
    }
}

pub mod connect {
    use futures::{Future, Poll};
    use std::time::Duration;

    use super::SetKeepalive;
    use svc;
    use transport::connect;

    pub fn layer(keepalive: Option<Duration>) -> Layer {
        Layer { keepalive }
    }

    #[derive(Clone, Debug)]
    pub struct Layer {
        keepalive: Option<Duration>,
    }

    #[derive(Clone, Debug)]
    pub struct Stack<M> {
        keepalive: Option<Duration>,
        inner: M,
    }

    #[derive(Clone, Debug)]
    pub struct Connect<T> {
        keepalive: Option<Duration>,
        inner: T,
    }

    impl<T, M> svc::Layer<T, T, M> for Layer
    where
        M: svc::Stack<T>,
        M::Value: connect::Connect,
        <M::Value as connect::Connect>::Connected: SetKeepalive,
    {
        type Value = <Stack<M> as svc::Stack<T>>::Value;
        type Error = <Stack<M> as svc::Stack<T>>::Error;
        type Stack = Stack<M>;

        fn bind(&self, inner: M) -> Self::Stack {
            Stack {
                inner,
                keepalive: self.keepalive,
            }
        }
    }

    impl<T, M> svc::Stack<T> for Stack<M>
    where
        M: svc::Stack<T>,
        M::Value: connect::Connect,
        <M::Value as connect::Connect>::Connected: SetKeepalive,
    {
        type Value = Connect<M::Value>;
        type Error = M::Error;

        fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
            let inner = self.inner.make(target)?;
            Ok(Connect {
                inner,
                keepalive: self.keepalive,
            })
        }
    }

    impl<C> connect::Connect for Connect<C>
    where
        C: connect::Connect,
        C::Connected: SetKeepalive,
    {
        type Connected = C::Connected;
        type Error = C::Error;
        type Future = Connect<C::Future>;

        fn connect(&self) -> Self::Future {
            let keepalive = self.keepalive;
            let inner = self.inner.connect();
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

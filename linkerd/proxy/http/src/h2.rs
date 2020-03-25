use super::Body;
use futures::{try_ready, Future, Poll};
use http;
use hyper::{
    body::Payload,
    client::conn::{self, Handshake, SendRequest},
};
use linkerd2_error::Error;
use std::marker::PhantomData;
use tokio::executor::{DefaultExecutor, Executor};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, info_span};
use tracing_futures::Instrument;

#[derive(Copy, Clone, Debug, Default)]
pub struct Settings {
    pub initial_stream_window_size: Option<u32>,
    pub initial_connection_window_size: Option<u32>,
}

#[derive(Debug)]
pub struct Connect<C, B> {
    connect: C,
    h2_settings: Settings,
    _marker: PhantomData<fn() -> B>,
}

#[derive(Debug)]
pub struct Connection<B> {
    tx: SendRequest<B>,
}

pub struct ConnectFuture<F: Future, B> {
    state: ConnectState<F, B>,
    h2_settings: Settings,
}

enum ConnectState<F: Future, B> {
    Connect(F),
    Handshake(Handshake<F::Item, B>),
}

pub struct ResponseFuture {
    inner: conn::ResponseFuture,
}

// ===== impl Connect =====

impl<C, B> Connect<C, B> {
    pub fn new(connect: C, h2_settings: Settings) -> Self {
        Connect {
            connect,
            h2_settings,
            _marker: PhantomData,
        }
    }
}

impl<C: Clone, B> Clone for Connect<C, B> {
    fn clone(&self) -> Self {
        Connect {
            connect: self.connect.clone(),
            h2_settings: self.h2_settings.clone(),
            _marker: PhantomData,
        }
    }
}

impl<C, B, T> tower::Service<T> for Connect<C, B>
where
    C: tower::MakeConnection<T>,
    C::Connection: Send + 'static,
    C::Error: Into<Error>,
    B: Payload,
{
    type Response = Connection<B>;
    type Error = Error;
    type Future = ConnectFuture<C::Future, B>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.connect.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, target: T) -> Self::Future {
        ConnectFuture {
            state: ConnectState::Connect(self.connect.make_connection(target)),
            h2_settings: self.h2_settings,
        }
    }
}

// ===== impl ConnectFuture =====

impl<F, B> Future for ConnectFuture<F, B>
where
    F: Future,
    F::Item: AsyncRead + AsyncWrite + Send + 'static,
    F::Error: Into<Error>,
    B: Payload,
{
    type Item = Connection<B>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            self.state = match self.state {
                ConnectState::Connect(ref mut fut) => {
                    let io = try_ready!(fut.poll().map_err(Into::into));
                    let hs = conn::Builder::new()
                        .executor(DefaultExecutor::current().instrument(info_span!("h2")))
                        .http2_only(true)
                        .http2_initial_stream_window_size(
                            self.h2_settings.initial_stream_window_size,
                        )
                        .http2_initial_connection_window_size(
                            self.h2_settings.initial_connection_window_size,
                        )
                        .handshake(io);

                    ConnectState::Handshake(hs)
                }
                ConnectState::Handshake(ref mut hs) => {
                    let (tx, conn) = try_ready!(hs.poll());

                    DefaultExecutor::current()
                        .instrument(info_span!("h2"))
                        .spawn(Box::new(conn.map_err(|error| debug!(%error, "failed"))))
                        .map_err(Error::from)?;

                    return Ok(Connection { tx }.into());
                }
            };
        }
    }
}

// ===== impl Connection =====

impl<B> tower::Service<http::Request<B>> for Connection<B>
where
    B: Payload,
{
    type Response = http::Response<Body>;
    type Error = hyper::Error;
    type Future = ResponseFuture;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.tx.poll_ready().map_err(From::from)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        debug_assert_eq!(
            req.version(),
            http::Version::HTTP_2,
            "request version should be HTTP/2",
        );

        // A request translated from HTTP/1 to 2 might not include an
        // authority. In order to support that case, our h2 library requires
        // the version to be dropped down from HTTP/2, as a form of us
        // explicitly acknowledging that its not a normal HTTP/2 form.
        if req.uri().authority_part().is_none() {
            *req.version_mut() = http::Version::HTTP_11;
        }

        ResponseFuture {
            inner: self.tx.send_request(req),
        }
    }
}

// ===== impl ResponseFuture =====

impl Future for ResponseFuture {
    type Item = http::Response<Body>;
    type Error = hyper::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let res = try_ready!(self.inner.poll());
        let res = res.map(|body| Body {
            body: Some(body),
            upgrade: None,
        });
        Ok(res.into())
    }
}

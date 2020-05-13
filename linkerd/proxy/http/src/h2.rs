use super::Body;
use futures_03::{ready, TryFuture, TryFutureExt};
use http;
use hyper::{
    body::HttpBody,
    client::conn::{self, SendRequest},
};
use linkerd2_error::Error;
use pin_project::{pin_project, project};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio_02::io::{AsyncRead, AsyncWrite};
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

#[pin_project]
pub struct ConnectFuture<F, B>
where
    F: TryFuture,
    B: HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync,
    F::Ok: AsyncRead + AsyncWrite + Send + 'static,
{
    #[pin]
    state: ConnectState<F, B>,
    h2_settings: Settings,
}

#[pin_project]
enum ConnectState<F, B>
where
    F: TryFuture,
    B: HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync,
    F::Ok: AsyncRead + AsyncWrite + Send + 'static,
{
    Connect(#[pin] F),
    Handshake(
        #[pin]
        Pin<
            Box<
                dyn Future<
                    Output = hyper::Result<(
                        SendRequest<B>,
                        hyper::client::conn::Connection<F::Ok, B>,
                    )>,
                >,
            >,
        >,
    ),
}

#[pin_project]
pub struct ResponseFuture {
    #[pin]
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
    C: tower::make::MakeConnection<T>,
    C::Connection: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C::Error: Into<Error>,
    B: HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync,
{
    type Response = Connection<B>;
    type Error = Error;
    type Future = ConnectFuture<C::Future, B>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.connect.poll_ready(cx).map_err(Into::into)
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
    F: TryFuture,
    F::Ok: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F::Error: Into<Error>,
    B: HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync,
{
    type Output = Result<Connection<B>, Error>;

    #[project]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        loop {
            #[project]
            match this.state.as_mut().project() {
                ConnectState::Connect(fut) => {
                    let io = ready!(fut.try_poll(cx)).map_err(Into::into)?;
                    let hs = conn::Builder::new()
                        .http2_only(true)
                        .http2_initial_stream_window_size(
                            this.h2_settings.initial_stream_window_size,
                        )
                        .http2_initial_connection_window_size(
                            this.h2_settings.initial_connection_window_size,
                        )
                        .handshake(io)
                        .instrument(info_span!("h2"));

                    this.state.set(ConnectState::Handshake(Box::pin(hs)));
                }
                ConnectState::Handshake(hs) => {
                    let (tx, conn) = ready!(hs.poll(cx))?;

                    tokio_02::spawn(
                        conn.map_err(|error| debug!(%error, "failed").instrument(info_span!("h2"))),
                    );

                    return Poll::Ready(Ok(Connection { tx }));
                }
            };
        }
    }
}

// ===== impl Connection =====

impl<B> tower::Service<http::Request<B>> for Connection<B>
where
    B: HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync,
{
    type Response = http::Response<Body>;
    type Error = hyper::Error;
    type Future = ResponseFuture;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.tx.poll_ready(cx).map_err(From::from)
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
        if req.uri().authority().is_none() {
            *req.version_mut() = http::Version::HTTP_11;
        }

        ResponseFuture {
            inner: self.tx.send_request(req),
        }
    }
}

// ===== impl ResponseFuture =====

impl Future for ResponseFuture {
    type Output = Result<http::Response<Body>, hyper::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = ready!(self.project().inner.poll(cx))?;
        let res = res.map(|body| Body {
            body: Some(body),
            upgrade: None,
        });
        Poll::Ready(Ok(res))
    }
}

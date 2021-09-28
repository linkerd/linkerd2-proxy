use crate::trace;
use futures::prelude::*;
pub use h2::{Error as H2Error, Reason};
use hyper::{
    body::HttpBody,
    client::conn::{self, SendRequest},
};
use linkerd_error::{Error, Result};
use std::time::Duration;
use std::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::instrument::Instrument;
use tracing::{debug, debug_span, trace_span};

#[derive(Copy, Clone, Debug, Default)]
pub struct Settings {
    pub initial_stream_window_size: Option<u32>,
    pub initial_connection_window_size: Option<u32>,
    pub keepalive_timeout: Option<Duration>,
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

// === impl Connect ===

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
            h2_settings: self.h2_settings,
            _marker: PhantomData,
        }
    }
}

type ConnectFuture<B> = Pin<Box<dyn Future<Output = Result<Connection<B>>> + Send + 'static>>;

impl<C, B, T> tower::Service<T> for Connect<C, B>
where
    C: tower::make::MakeConnection<T>,
    C::Future: Send + 'static,
    C::Connection: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C::Error: Into<Error>,
    B: HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: Into<Error> + Send + Sync,
{
    type Response = Connection<B>;
    type Error = Error;
    type Future = ConnectFuture<B>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.connect.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let Settings {
            initial_connection_window_size,
            initial_stream_window_size,
            keepalive_timeout,
        } = self.h2_settings;

        let connect = self
            .connect
            .make_connection(target)
            .instrument(trace_span!("connect"));

        Box::pin(
            async move {
                let io = connect.err_into::<Error>().await?;
                let mut builder = conn::Builder::new();
                builder
                    .http2_only(true)
                    .http2_initial_stream_window_size(initial_stream_window_size)
                    .http2_initial_connection_window_size(initial_connection_window_size)
                    .executor(trace::Executor::new());

                // Configure HTTP/2 PING frames
                if let Some(timeout) = keepalive_timeout {
                    // XXX(eliza): is this a reasonable interval between
                    // PING frames?
                    let interval = timeout / 4;
                    builder
                        .http2_keep_alive_timeout(timeout)
                        .http2_keep_alive_interval(interval)
                        .http2_keep_alive_while_idle(true);
                }

                let (tx, conn) = builder
                    .handshake(io)
                    .instrument(trace_span!("handshake"))
                    .await?;

                tokio::spawn(
                    conn.map_err(|error| debug!(%error, "failed"))
                        .instrument(trace_span!("conn").or_current()),
                );

                Ok(Connection { tx })
            }
            .instrument(debug_span!("h2")),
        )
    }
}

// === impl Connection ===

impl<B> tower::Service<http::Request<B>> for Connection<B>
where
    B: HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: Into<Error> + Send + Sync,
{
    type Response = http::Response<hyper::Body>;
    type Error = hyper::Error;
    type Future = conn::ResponseFuture;

    #[inline]
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

        self.tx.send_request(req)
    }
}

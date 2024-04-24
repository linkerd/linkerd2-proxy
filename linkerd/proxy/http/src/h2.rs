use crate::executor::TracingExecutor;
use futures::prelude::*;
pub use h2::{Error as H2Error, Reason};
use hyper::{
    body::HttpBody,
    client::conn::{self, SendRequest},
};
use linkerd_error::{Error, Result};
use linkerd_stack::{MakeConnection, Service};
use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tracing::instrument::Instrument;
use tracing::{debug, debug_span, trace_span};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerParams {
    pub flow_control: Option<FlowControl>,
    pub keep_alive: Option<KeepAlive>,
    pub max_concurrent_streams: Option<u32>,

    // Internals
    pub max_frame_size: Option<u32>,
    pub max_header_list_size: Option<u32>,
    pub max_pending_accept_reset_streams: Option<usize>,
    pub max_send_buf_size: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientParams {
    pub flow_control: Option<FlowControl>,
    pub keep_alive: Option<ClientKeepAlive>,

    // Internals
    pub max_concurrent_reset_streams: Option<usize>,
    pub max_frame_size: Option<u32>,
    pub max_send_buf_size: Option<usize>,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct KeepAlive {
    pub interval: Duration,
    pub timeout: Duration,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientKeepAlive {
    pub interval: Duration,
    pub timeout: Duration,
    pub while_idle: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FlowControl {
    Adaptive,
    Fixed {
        initial_stream_window_size: u32,
        initial_connection_window_size: u32,
    },
}

#[derive(Debug)]
pub struct Connect<C, B> {
    connect: C,
    params: ClientParams,
    _marker: PhantomData<fn() -> B>,
}

#[derive(Debug)]
pub struct Connection<B> {
    tx: SendRequest<B>,
}

// === impl Connect ===

impl<C, B> Connect<C, B> {
    pub fn new(connect: C, params: ClientParams) -> Self {
        Connect {
            connect,
            params,
            _marker: PhantomData,
        }
    }
}

impl<C: Clone, B> Clone for Connect<C, B> {
    fn clone(&self) -> Self {
        Connect {
            connect: self.connect.clone(),
            params: self.params.clone(),
            _marker: PhantomData,
        }
    }
}

type ConnectFuture<B> = Pin<Box<dyn Future<Output = Result<Connection<B>>> + Send + 'static>>;

impl<C, B, T> Service<T> for Connect<C, B>
where
    C: MakeConnection<(crate::Version, T)>,
    C::Connection: Send + Unpin + 'static,
    C::Metadata: Send,
    C::Future: Send + 'static,
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
        let ClientParams {
            flow_control,
            keep_alive,
            max_concurrent_reset_streams,
            max_frame_size,
            max_send_buf_size,
        } = self.params;

        let connect = self
            .connect
            .connect((crate::Version::H2, target))
            .instrument(trace_span!("connect").or_current());

        Box::pin(
            async move {
                let (io, _meta) = connect.err_into::<Error>().await?;
                let mut builder = conn::Builder::new();
                builder.executor(TracingExecutor).http2_only(true);
                match flow_control {
                    None => {}
                    Some(FlowControl::Adaptive) => {
                        builder.http2_adaptive_window(true);
                    }
                    Some(FlowControl::Fixed {
                        initial_stream_window_size,
                        initial_connection_window_size,
                    }) => {
                        builder
                            .http2_initial_stream_window_size(initial_stream_window_size)
                            .http2_initial_connection_window_size(initial_connection_window_size);
                    }
                }

                // Configure HTTP/2 PING frames
                if let Some(ClientKeepAlive {
                    timeout,
                    interval,
                    while_idle,
                }) = keep_alive
                {
                    builder
                        .http2_keep_alive_timeout(timeout)
                        .http2_keep_alive_interval(interval)
                        .http2_keep_alive_while_idle(while_idle);
                }

                builder.http2_max_frame_size(max_frame_size);
                if let Some(max) = max_concurrent_reset_streams {
                    builder.http2_max_concurrent_reset_streams(max);
                }
                if let Some(sz) = max_send_buf_size {
                    builder.http2_max_send_buf_size(sz);
                }

                let (tx, conn) = builder
                    .handshake(io)
                    .instrument(trace_span!("handshake").or_current())
                    .await?;

                tokio::spawn(
                    conn.map_err(|error| debug!(%error, "failed"))
                        .instrument(trace_span!("conn").or_current()),
                );

                Ok(Connection { tx })
            }
            .instrument(debug_span!("h2").or_current()),
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

use futures::{Future, Poll, Stream};
use http::HeaderMap;
use prost::Message;
use std::{
    fmt,
    sync::Weak,
};
use tower_http::{HttpService};
use tower_grpc::{
    self as grpc,
    Body,
    BoxBody,
    Streaming,
    client::server_streaming::ResponseFuture,
};

/// Tracks the state of a gRPC response stream from a remote.
///
/// A remote may hold a `Receiver` that can be used to read `M`-typed messages from the
/// remote stream.
pub enum Remote<M, S>
where
    S: HttpService<BoxBody>,
    S::ResponseBody: Body,
{
    NeedsReconnect,
    ConnectedOrConnecting {
        rx: Receiver<M, S>,
    },
}

/// Receives streaming RPCs updates.
///
/// Streaming gRPC endpoints return a `ResponseFuture` whose item is a `Response<Stream>`.
/// A `Receiver` holds the state of that RPC call, exposing a `Stream` that drives both
/// the gRPC response and its streaming body.
pub struct Receiver<M, S>
where
    S: HttpService<BoxBody>,
    S::ResponseBody: Body,
{
    rx: Rx<M, S>,

    /// Used by `background::NewQuery` for counting the number of currently
    /// active queries that it has created.
    _active: Weak<()>,
}

enum Rx<M, S>
where
    S: HttpService<BoxBody>,
    S::ResponseBody: Body,
{
    Waiting(ResponseFuture<M, S::Future>),
    Streaming(Streaming<M, S::ResponseBody>),
}

// ===== impl Receiver =====

impl<M: Message + Default, S: HttpService<BoxBody>> Receiver<M, S>
where
    S::ResponseBody: Body,
    S::Error: fmt::Debug,
{
    pub fn new(future: ResponseFuture<M, S::Future>, active: Weak<()>) -> Self {
        Receiver {
            rx: Rx::Waiting(future),
            _active: active,
        }
    }

    // Coerces the stream's Error<()> to an Error<S::Error>.
    fn coerce_stream_err(e: grpc::Error<()>) -> grpc::Error<S::Error> {
        match e {
            grpc::Error::Grpc(s, h) => grpc::Error::Grpc(s, h),
            grpc::Error::Decode(e) => grpc::Error::Decode(e),
            grpc::Error::Protocol(e) => grpc::Error::Protocol(e),
            grpc::Error::Inner(()) => {
                // `stream.poll` shouldn't return this variant. If it for
                // some reason does, we report this as an unknown error.
                warn!("unexpected gRPC stream error");
                debug_assert!(false);
                grpc::Error::Grpc(grpc::Status::with_code(grpc::Code::Unknown), HeaderMap::new())
            }
        }
    }
}

impl<M: Message + Default, S: HttpService<BoxBody>> Stream for Receiver<M, S>
where
    S::ResponseBody: Body,
    S::Error: fmt::Debug,
{
    type Item = M;
    type Error = grpc::Error<S::Error>;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            let stream = match self.rx {
                Rx::Waiting(ref mut future) => {
                    try_ready!(future.poll()).into_inner()
                }

                Rx::Streaming(ref mut stream) => {
                    return stream.poll().map_err(Self::coerce_stream_err);
                }
            };

            self.rx = Rx::Streaming(stream);
        }
    }
}

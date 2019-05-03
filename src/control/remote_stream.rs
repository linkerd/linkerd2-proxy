use futures::{Future, Poll, Stream};
use prost::Message;
use tower_grpc::{
    self as grpc, client::server_streaming::ResponseFuture, generic::client::GrpcService, BoxBody,
    Streaming,
};

/// Tracks the state of a gRPC response stream from a remote.
///
/// A remote may hold a `Receiver` that can be used to read `M`-typed messages from the
/// remote stream.
pub enum Remote<M, S>
where
    S: GrpcService<BoxBody>,
{
    NeedsReconnect,
    ConnectedOrConnecting { rx: Receiver<M, S> },
}

/// Receives streaming RPCs updates.
///
/// Streaming gRPC endpoints return a `ResponseFuture` whose item is a `Response<Stream>`.
/// A `Receiver` holds the state of that RPC call, exposing a `Stream` that drives both
/// the gRPC response and its streaming body.
pub struct Receiver<M, S>
where
    S: GrpcService<BoxBody>,
{
    rx: Rx<M, S>,
}

enum Rx<M, S>
where
    S: GrpcService<BoxBody>,
{
    Waiting(ResponseFuture<M, S::Future>),
    Streaming(Streaming<M, S::ResponseBody>),
}

// ===== impl Receiver =====

impl<M: Message + Default, S: GrpcService<BoxBody>> Receiver<M, S> {
    pub fn new(future: ResponseFuture<M, S::Future>) -> Self {
        Receiver {
            rx: Rx::Waiting(future),
        }
    }
}

impl<M: Message + Default, S: GrpcService<BoxBody>> Stream for Receiver<M, S> {
    type Item = M;
    type Error = grpc::Status;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            let stream = match self.rx {
                Rx::Waiting(ref mut future) => try_ready!(future.poll()).into_inner(),

                Rx::Streaming(ref mut stream) => {
                    return stream.poll();
                }
            };

            self.rx = Rx::Streaming(stream);
        }
    }
}

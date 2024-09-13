use crate::grpc::Server;
use futures::future;
use linkerd2_proxy_api::tap::tap_server::{Tap, TapServer};
use linkerd_conditional::Conditional;
use linkerd_error::Error;
use linkerd_io as io;
use linkerd_meshtls as meshtls;
use linkerd_proxy_http::TracingExecutor;
use linkerd_tls as tls;
use std::{
    collections::HashSet,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tower::Service;

#[derive(Clone, Debug)]
pub struct AcceptPermittedClients {
    permitted_client_ids: Arc<HashSet<tls::ClientId>>,
    server: Server,
}

type Connection<T, I> = (
    (tls::ConditionalServerTls, T),
    io::EitherIo<meshtls::ServerIo<tls::server::DetectIo<I>>, tls::server::DetectIo<I>>,
);

pub type ServeFuture = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

impl AcceptPermittedClients {
    pub fn new(permitted_client_ids: Arc<HashSet<tls::ClientId>>, server: Server) -> Self {
        Self {
            permitted_client_ids,
            server,
        }
    }

    fn serve<I, T>(&self, io: I, tap: T) -> ServeFuture
    where
        I: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
        T: Tap + Send + 'static,
        T::ObserveStream: Send + 'static,
    {
        let svc = TapServer::new(tap);
        Box::pin(async move {
            hyper::server::conn::Http::new()
                .with_executor(TracingExecutor)
                .http2_only(true)
                .serve_connection(io, svc)
                .await
                .map_err(Into::into)
        })
    }

    fn serve_authenticated<I>(&self, io: I) -> ServeFuture
    where
        I: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
    {
        self.serve(io, self.server.clone())
    }

    fn serve_unauthenticated<I>(&self, io: I, msg: impl Into<String>) -> ServeFuture
    where
        I: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
    {
        self.serve(io, unauthenticated::new(msg))
    }
}

impl<T, I> Service<Connection<T, I>> for AcceptPermittedClients
where
    I: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
{
    type Response = ServeFuture;
    type Error = Error;
    type Future = future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, conn: Connection<T, I>) -> Self::Future {
        match conn {
            ((Conditional::Some(tls), _), io) => {
                if let tls::ServerTls::Established {
                    client_id: Some(id),
                    ..
                } = tls
                {
                    if self.permitted_client_ids.contains(&id) {
                        return future::ok(Box::pin(self.serve_authenticated(io)));
                    }
                }

                future::ok(Box::pin(
                    self.serve_unauthenticated(io, "Unauthorized client"),
                ))
            }
            ((Conditional::None(tls::NoServerTls::Loopback), _), io) => {
                future::ok(Box::pin(self.serve_authenticated(io)))
            }
            ((Conditional::None(reason), _), io) => {
                future::ok(Box::pin(self.serve_unauthenticated(io, reason.to_string())))
            }
        }
    }
}

pub mod unauthenticated {
    use futures::stream;
    use linkerd2_proxy_api::tap as api;
    use tonic::{Code, Request, Response, Status};

    #[derive(Clone, Debug, Default)]
    pub struct Unauthenticated(String);

    pub fn new(message: impl Into<String>) -> Unauthenticated {
        Unauthenticated(message.into())
    }

    #[tonic::async_trait]
    impl api::tap_server::Tap for Unauthenticated {
        type ObserveStream = stream::Empty<Result<api::TapEvent, Status>>;

        async fn observe(
            &self,
            _req: Request<api::ObserveRequest>,
        ) -> Result<Response<Self::ObserveStream>, Status> {
            Err(Status::new(Code::Unauthenticated, &self.0))
        }
    }
}

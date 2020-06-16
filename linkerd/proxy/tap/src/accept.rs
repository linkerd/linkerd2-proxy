use futures::future;
use indexmap::IndexSet;
use linkerd2_error::Error;
use linkerd2_identity as identity;
use linkerd2_proxy_api::tap::tap_server::{Tap, TapServer};
use linkerd2_proxy_http::{trace, HyperServerSvc};
use linkerd2_proxy_transport::io::BoxedIo;
use linkerd2_proxy_transport::tls::{
    accept::Connection, Conditional, ReasonForNoIdentity, ReasonForNoPeerName,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tower::Service;

#[derive(Clone, Debug)]
pub struct AcceptPermittedClients {
    permitted_client_ids: Arc<IndexSet<identity::Name>>,
    server: super::Server,
}

pub type ServeFuture = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

impl AcceptPermittedClients {
    pub fn new(permitted_client_ids: Arc<IndexSet<identity::Name>>, server: super::Server) -> Self {
        Self {
            permitted_client_ids,
            server,
        }
    }

    fn serve<T>(&self, io: BoxedIo, tap: T) -> ServeFuture
    where
        T: Tap + Send + 'static,
        T::ObserveStream: Send + 'static,
    {
        let svc = TapServer::new(tap);
        Box::pin(async move {
            hyper::server::conn::Http::new()
                .with_executor(trace::Executor::new())
                .http2_only(true)
                .serve_connection(io, HyperServerSvc::new(svc))
                .await
                .map_err(Into::into)
        })
    }

    fn serve_authenticated(&self, io: BoxedIo) -> ServeFuture {
        self.serve(io, self.server.clone())
    }

    fn serve_unauthenticated(&self, io: BoxedIo, msg: impl Into<String>) -> ServeFuture {
        self.serve(io, unauthenticated::new(msg))
    }
}

impl Service<Connection> for AcceptPermittedClients {
    type Response = ServeFuture;
    type Error = Error;
    type Future = future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, (meta, io): Connection) -> Self::Future {
        future::ok(match meta.peer_identity {
            Conditional::Some(ref peer) => {
                if self.permitted_client_ids.contains(peer) {
                    Box::pin(self.serve_authenticated(io))
                } else {
                    Box::pin(self.serve_unauthenticated(io, format!("Unauthorized peer: {}", peer)))
                }
            }
            Conditional::None(ReasonForNoIdentity::NoPeerName(ReasonForNoPeerName::Loopback)) => {
                Box::pin(self.serve_authenticated(io))
            }
            Conditional::None(reason) => {
                Box::pin(self.serve_unauthenticated(io, reason.to_string()))
            }
        })
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

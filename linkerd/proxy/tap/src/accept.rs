use futures::{future, Future, Poll};
use indexmap::IndexSet;
use linkerd2_error::Error;
use linkerd2_identity as identity;
use linkerd2_proxy_api::tap::server::{Tap, TapServer};
use linkerd2_proxy_http::{
    grpc::{req_box_body, res_body_as_payload},
    HyperServerSvc,
};
use linkerd2_proxy_transport::io::BoxedIo;
use linkerd2_proxy_transport::tls::{
    accept::Connection, Conditional, ReasonForNoIdentity, ReasonForNoPeerName,
};
use std::sync::Arc;
use tower::Service;

#[derive(Clone, Debug)]
pub struct AcceptPermittedClients {
    permitted_client_ids: Arc<IndexSet<identity::Name>>,
    server: super::Server,
}

pub struct ServeFuture(Box<dyn Future<Item = (), Error = Error> + Send + 'static>);

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
        T::ObserveFuture: Send + 'static,
        T::ObserveStream: Send + 'static,
    {
        let svc =
            res_body_as_payload::Service::new(req_box_body::Service::new(TapServer::new(tap)));

        // TODO do we need to set a contextual tracing executor on Hyper?
        ServeFuture(Box::new(
            hyper::server::conn::Http::new()
                .http2_only(true)
                .serve_connection(io, HyperServerSvc::new(svc))
                .map_err(Into::into),
        ))
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
    type Future = future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    fn call(&mut self, (meta, io): Connection) -> Self::Future {
        future::ok(match meta.peer_identity {
            Conditional::Some(ref peer) => {
                if self.permitted_client_ids.contains(peer) {
                    self.serve_authenticated(io)
                } else {
                    self.serve_unauthenticated(io, format!("Unauthorized peer: {}", peer))
                }
            }
            Conditional::None(ReasonForNoIdentity::NoPeerName(ReasonForNoPeerName::Loopback)) => {
                self.serve_authenticated(io)
            }
            Conditional::None(reason) => self.serve_unauthenticated(io, reason.to_string()),
        })
    }
}

impl Future for ServeFuture {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<(), Self::Error> {
        self.0.poll()
    }
}

pub mod unauthenticated {
    use futures::{future, stream};
    use linkerd2_proxy_api::tap as api;
    use tower_grpc::{Code, Request, Response, Status};

    #[derive(Clone, Debug, Default)]
    pub struct Unauthenticated(String);

    pub fn new(message: impl Into<String>) -> Unauthenticated {
        Unauthenticated(message.into())
    }

    impl api::server::Tap for Unauthenticated {
        type ObserveStream = stream::Empty<api::TapEvent, Status>;
        type ObserveFuture = future::FutureResult<Response<Self::ObserveStream>, Status>;

        fn observe(&mut self, _req: Request<api::ObserveRequest>) -> Self::ObserveFuture {
            future::err(Status::new(Code::Unauthenticated, &self.0))
        }
    }
}

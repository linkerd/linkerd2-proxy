use crate::api::tap::server::TapServer;
use crate::identity;
use crate::proxy::grpc::{req_box_body, res_body_as_payload};
use crate::proxy::http::HyperServerSvc;
use crate::svc::Service;
use crate::transport::tls::{self, HasPeerIdentity};
use crate::Error;
use futures::{Future, Poll};
use indexmap::IndexSet;

#[derive(Clone, Debug)]
pub struct AcceptPermittedClients {
    permitted_client_ids: IndexSet<identity::Name>,
    server: TapServer<super::Server>,
}

pub enum ServeFuture {
    Failed(Option<Error>),
    Serve(Box<dyn Future<Item = (), Error = Error> + Send + 'static>),
}

#[derive(Debug)]
pub enum AcceptError {
    Unauthorized(identity::Name),
    NoPeerIdentity(tls::ReasonForNoIdentity),
}

impl AcceptPermittedClients {
    pub fn new<I: Iterator<Item = identity::Name>>(
        permitted_ids: I,
        server: super::Server,
    ) -> Self {
        Self {
            permitted_client_ids: permitted_ids.collect(),
            server: TapServer::new(server),
        }
    }

    fn serve(&mut self, conn: tls::Connection) -> ServeFuture {
        let svc =
            res_body_as_payload::Service::new(req_box_body::Service::new(self.server.clone()));
        let fut = hyper::server::conn::Http::new()
            //.with_executor(log2.executor())
            .http2_only(true)
            .serve_connection(conn, HyperServerSvc::new(svc))
            .map_err(Into::into);
        ServeFuture::Serve(Box::new(fut))
    }
}

impl Service<tls::Connection> for AcceptPermittedClients {
    type Response = ();
    type Error = Error;
    type Future = ServeFuture;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    fn call(&mut self, conn: tls::Connection) -> Self::Future {
        match conn.peer_identity() {
            tls::Conditional::Some(ref peer) if self.permitted_client_ids.contains(peer) => {
                self.serve(conn)
            }
            tls::Conditional::None(tls::ReasonForNoIdentity::NoPeerName(
                tls::ReasonForNoPeerName::Loopback,
            )) => self.serve(conn),
            tls::Conditional::Some(peer) => {
                ServeFuture::Failed(Some(AcceptError::Unauthorized(peer).into()))
            }
            tls::Conditional::None(reason) => {
                ServeFuture::Failed(Some(AcceptError::NoPeerIdentity(reason).into()))
            }
        }
    }
}

impl Future for ServeFuture {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<(), Self::Error> {
        return match self {
            ServeFuture::Serve(ref mut f) => f.poll(),
            ServeFuture::Failed(ref mut e) => Err(e.take().unwrap().into()),
        };
    }
}

impl std::fmt::Display for AcceptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcceptError::Unauthorized(peer) => write!(f, "unauthorized peer: {}", peer),
            AcceptError::NoPeerIdentity(reason) => write!(f, "no peer identity: {}", reason),
        }
    }
}

impl std::error::Error for AcceptError {}

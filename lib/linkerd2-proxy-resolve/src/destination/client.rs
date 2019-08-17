use super::super::remote_stream::{self, Remote};
use crate::addr::NameAddr;
use crate::api::destination::{client::Destination, GetDestination, Update};
use futures::{Async, Poll, Stream};
use std::sync::Arc;
use tower_grpc::{self as grpc, generic::client::GrpcService, BoxBody};
use tracing::trace;

/// A client for making service discovery requests to the destination service.
#[derive(Clone)]
pub struct Client<T> {
    client: T,
    context_token: Arc<String>,
}

/// A destination service query for a particular name.
///
/// A `Query` manages the underlying gRPC request and can reconnect itself as necessary.
pub struct Query<T>
where
    T: GrpcService<BoxBody>,
{
    auth: NameAddr,
    client: Client<T>,
    query: remote_stream::Remote<Update, T>,
}

// ===== impl Client =====

impl<T> Client<T>
where
    T: GrpcService<BoxBody>,
{
    fn query(&mut self, dst: &NameAddr, kind: &str) -> Remote<Update, T> {
        trace!("{}ing destination service query for {}", kind, dst);
        if let Ok(Async::Ready(())) = self.client.poll_ready() {
            let req = GetDestination {
                scheme: "k8s".into(),
                path: format!("{}", dst),
                context_token: self.context_token.as_ref().clone(),
            };
            let mut svc = Destination::new(self.client.as_service());
            let response = svc.get(grpc::Request::new(req));
            let rx = remote_stream::Receiver::new(response);
            Remote::ConnectedOrConnecting { rx }
        } else {
            trace!("destination client not yet ready");
            Remote::NeedsReconnect
        }
    }

    /// Returns a destination service query for the given `dst`.
    pub fn connect(mut self, dst: &NameAddr) -> Query<T> {
        let query = self.query(dst, "connect");
        Query {
            auth: dst.clone(),
            client: self,
            query,
        }
    }

    pub fn new(client: T, proxy_id: String) -> Self {
        Self {
            client,
            context_token: Arc::new(proxy_id),
        }
    }
}

impl<T> Query<T>
where
    T: GrpcService<BoxBody>,
{
    pub fn authority(&self) -> &NameAddr {
        &self.auth
    }

    /// Indicates that this query should be reconnected.
    pub fn reconnect(&mut self) {
        self.query = Remote::NeedsReconnect;
    }

    /// Polls the destination service query for updates, reconnecting if necessary.
    pub fn poll(&mut self) -> Poll<Option<Update>, grpc::Status> {
        loop {
            self.query = match self.query {
                Remote::ConnectedOrConnecting { ref mut rx } => return rx.poll(),
                Remote::NeedsReconnect => match self.client.query(&self.auth, "reconnect") {
                    Remote::NeedsReconnect => return Ok(Async::NotReady),
                    query => query,
                },
            }
        }
    }
}

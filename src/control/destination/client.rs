use futures::{Async, Poll, Stream};

use tower_grpc::{self as grpc, generic::client::GrpcService, BoxBody};

use api::destination::{client::Destination, GetDestination, Update};

use control::remote_stream::{self, Remote};
use std::sync::Arc;

use NameAddr;

#[derive(Clone)]
pub struct Client<T> {
    client: T,
    context_token: Arc<String>,
}

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
    pub fn connect(self, dst: &NameAddr) -> Query<T> {
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

    pub fn reconnect(&mut self) {
        self.query = Remote::NeedsReconnect;
    }

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

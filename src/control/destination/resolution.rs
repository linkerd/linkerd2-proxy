use indexmap::IndexMap;
use std::{collections::HashMap, iter::IntoIterator, net::SocketAddr};

use futures::{Async, Stream};
use tower_grpc::{generic::client::GrpcService, BoxBody};

use api::{
    destination::{
        protocol_hint::Protocol, update::Update as PbUpdate2, TlsIdentity, WeightedAddr,
    },
    net::TcpAddress,
};

use control::{
    cache::{Cache, CacheChange, Exists},
    destination::{Metadata, ProtocolHint, Responder, Update},
    remote_stream::Remote,
};

use identity;
use proxy::resolve;
use NameAddr;

use super::Client;

/// Holds the state of a single resolution.
pub struct Resolution<T>
where
    T: GrpcService<BoxBody>,
{
    updates: VecDeque<Update<Metadata>>,
    client: Client<T>,
    query: Option<Query<T>>,
    auth: NameAddr,
}

type Query<T> = Remote<PbUpdate, T>;

impl<T> resolve::Resolution for Resolution<T>
where
    T: GrpcService<BoxBody>,
{
    type Endpoint = Metadata;
    type Error = Never;

    fn poll(&mut self) -> Poll<Update<Self::Endpoint>, Self::Error> {
        let auth = &self.auth;
        let client = &mut self.client;
        let queue = &mut self.queue;
        loop {
            if let Some(update) = queue.pop_front() {
                return Ok(Async::Ready(update));
            }

            self.query = match self.query {
                Some(Remote::ConnectedOrConnecting { rx }) => match rx.poll() {
                    Ok(Async::Ready(Some(update))) => {
                        match update.update {
                            Some(PbUpdate2::Add(a_set)) => {
                                let set_labels = a_set.metric_labels;
                                let addrs = a_set
                                    .addrs
                                    .into_iter()
                                    .filter_map(|pb| pb_to_addr_meta(pb, &set_labels))
                                    .map(|addr, meta| Update::Add(addr, meta));
                                queue.extend(addrs);
                            }
                            Some(PbUpdate2::Remove(r_set)) => {
                                let addrs = r_set.addrs.into_iter().filter_map(pb_to_sock_addr).map(Update::Remove);
                                queue.extend(addrs);
                            }
                            Some(PbUpdate2::NoEndpoints(_)) => {
                                queue.clear();
                                queue.push_front(Update::NoEndpoints);
                                // TODO: queue removals for any existing eps.
                            }
                            None => (),
                        },
                        Some(Remote::ConnectedOrConnecting { rx })
                    }
                    Ok(Async::Ready(None)) => {
                        trace!(
                            "Destination.Get stream ended for {:?}, must reconnect",
                            auth
                        );
                        Some(Remote::NeedsReconnect)
                    }
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Err(ref status) if status.code() == tower_grpc::Code::InvalidArgument => {
                        // Invalid Argument is returned to indicate that the
                        // requested name should *not* query the destination
                        // service. In this case, do not attempt to reconnect.
                        debug!(
                            "Destination.Get stream ended for {:?} with Invalid Argument",
                            auth
                        );
                        queue.clear();
                        queue.push(Update::NoEndpoints);
                        // TODO: queue removals for any existing eps.
                        None
                    }
                    Err(err) => {
                        warn!("Destination.Get stream errored for {:?}: {:?}", auth, err);
                        Some(Remote::NeedsReconnect)
                    }
                },
                Some(Remote::NeedsReconnect) => {
                    client.query(auth, "reconnect")
                },
                None => return Ok(Async::NotReady),
            };

        }

    }
}gf

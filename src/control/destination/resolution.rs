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
    client: Client<T>,
    query: Option<Query<T>>,
    auth: NameAddr,
    cache: Cache,
}

#[derive(Debug)]
struct Cache {
    /// Used to "flatten" destination service responses containing multiple
    /// endpoints into a series of `destination::Update`s for single endpoints.
    queue: VecDeque<Update<Metadata>>,
    /// Tracks all the endpoint addresses we've seen, so that we can send
    /// `Update::Remove`s for them if we recieve a `NoEndpoints` response.
    addrs: IndexSet<SocketAddr>,
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
        let cache = &mut self.cache;
        loop {
            if let Some(update) = cache.next_update() {
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
                                    .filter_map(|pb| pb_to_addr_meta(pb, &set_labels));
                                cache.add(addrs);
                            }
                            Some(PbUpdate2::Remove(r_set)) => {
                                let addrs = r_set.addrs.into_iter().filter_map(pb_to_sock_addr);
                                cache.remove(addrs);
                            }
                            Some(PbUpdate2::NoEndpoints(_)) => cache.no_endpoints(),
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
                        cache.remove(addrs);
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
}


impl Cache {
    fn next_update(&mut self) -> Option<Update<Metadata>> {
        self.queue.pop_front()
    }

    fn add(&mut self, addrs: impl Iterator<Item = (SocketAddr, Metadata)>) {
        for (addr, meta) in addrs {
            self.queue.push_back(Update::Add(addr, meta));
            self.addrs.insert(addr);
        }
    }

    fn remove(&mut self, addrs: impl Iterator<Item = SocketAddr>) {
        for (addr, meta) in addrs {
            self.queue.push_back(Update::Remove(addr));
            self.addrs.remove(addr);
        }
    }

    fn no_endpoints(&mut self) {
        self.queue.clear();
        self.queue.push_front(Update::NoEndpoints);
        for addr in self.addrs.drain() {
            self.queue.push_back(Update::Remove(addr));
        }
    }
}

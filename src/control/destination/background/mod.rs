use futures::{sync::mpsc, Async, Future, Poll, Stream};
use std::{
    collections::{
        hash_map::{Entry, HashMap},
        VecDeque,
    },
    mem,
    time::Instant,
};
use tower_grpc::{self as grpc, generic::client::GrpcService, BoxBody};

use api::destination::client::Destination;
use api::destination::{GetDestination, Update as PbUpdate};

use super::{ResolveRequest, Update};
use control::{
    cache::Exists,
    remote_stream::{Receiver, Remote},
};
use dns;
use NameAddr;

mod destination_set;

use self::destination_set::DestinationSet;

type Query<T> = Remote<PbUpdate, T>;
type UpdateRx<T> = Receiver<PbUpdate, T>;

/// Satisfies resolutions as requested via `request_rx`.
///
/// As the `Background` is polled with a client to Destination service, if the client to the
/// service is healthy, it reads requests from `request_rx`, determines how to resolve the
/// provided authority to a set of addresses, and ensures that resolution updates are
/// propagated to all requesters.
pub struct Background<T>
where
    T: GrpcService<BoxBody>,
{
    new_query: NewQuery<T>,
    dsts: DestinationCache<T>,
    /// A receiver of new watch requests.
    request_rx: mpsc::UnboundedReceiver<ResolveRequest>,
}

/// Holds the currently active `DestinationSet`s and a list of any destinations
/// which require reconnects.
#[derive(Default)]
struct DestinationCache<T>
where
    T: GrpcService<BoxBody>,
{
    destinations: HashMap<NameAddr, DestinationSet<T>>,
    /// A queue of authorities that need to be reconnected.
    reconnects: VecDeque<NameAddr>,
}

/// The configurationn necessary to create a new Destination service
/// query.
struct NewQuery<T>
where
    T: GrpcService<BoxBody>,
{
    /// The Destination.Get RPC client service.
    client: Option<T>,
    suffixes: Vec<dns::Suffix>,
    context_token: String,
    /// Each poll, records whether the rpc service was till ready.
    rpc_ready: bool,
}

// ==== impl Background =====
impl<T> Future for Background<T>
where
    T: GrpcService<BoxBody>,
{
    type Item = ();
    type Error = ();
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // This loop is make sure any streams that were found disconnected
        // in `poll_destinations` while the `rpc` service is ready should
        // be reconnected now, otherwise the task would just sleep...
        loop {
            if let Async::Ready(()) = self.poll_resolve_requests() {
                // request_rx has closed, meaning the main thread is terminating.
                return Ok(Async::Ready(()));
            }
            self.dsts.retain_active();
            self.dsts.poll_destinations();

            if self.dsts.reconnects.is_empty() || !self.new_query.is_ready() {
                return Ok(Async::NotReady);
            }
        }
    }
}

impl<T> Background<T>
where
    T: GrpcService<BoxBody>,
{
    pub(super) fn new(
        request_rx: mpsc::UnboundedReceiver<ResolveRequest>,
        client: Option<T>,
        suffixes: Vec<dns::Suffix>,
        context_token: String,
    ) -> Self {
        Self {
            new_query: NewQuery {
                client,
                suffixes,
                context_token,
                rpc_ready: false,
            },
            dsts: DestinationCache::new(),
            request_rx,
        }
    }

    fn poll_resolve_requests(&mut self) -> Async<()> {
        loop {
            if let Async::NotReady = self.new_query.poll_ready() {
                return Async::NotReady;
            }

            // handle any pending reconnects first
            if self.dsts.poll_reconnect(&mut self.new_query) {
                continue;
            }

            // check for any new watches
            match self.request_rx.poll() {
                Ok(Async::Ready(Some(resolve))) => {
                    trace!("Destination.Get {:?}", resolve.authority);

                    let new_query = &mut self.new_query;
                    let dsts = &mut self.dsts;

                    match dsts.destinations.entry(resolve.authority) {
                        Entry::Occupied(mut occ) => {
                            occ.get_mut().add_responder(resolve.responder);
                        }
                        Entry::Vacant(vac) => {
                            let set = DestinationSet::new(vac.key(), resolve.responder, new_query);
                            vac.insert(set);
                        }
                    }
                }
                Ok(Async::Ready(None)) => {
                    trace!("Discover tx is dropped, shutdown");
                    return Async::Ready(());
                }
                Ok(Async::NotReady) => return Async::NotReady,
                Err(_) => unreachable!("unbounded receiver doesn't error"),
            }
        }
    }
}

// ===== impl NewQuery =====

impl<T> NewQuery<T>
where
    T: GrpcService<BoxBody>,
{
    fn poll_ready(&mut self) -> Async<()> {
        if let Some(ref mut client) = self.client {
            match client.poll_ready() {
                Ok(Async::Ready(())) => {
                    self.rpc_ready = true;
                    return Async::Ready(());
                }
                Ok(Async::NotReady) => {}
                Err(err) => warn!("Destination.Get poll_ready error: {:?}", err.into()),
            }
            self.rpc_ready = false;
            Async::NotReady
        } else {
            Async::Ready(())
        }
    }

    fn is_ready(&self) -> bool {
        self.rpc_ready
    }

    /// Attepts to initiate a query `query` to the Destination service
    /// if the given authority's host is of a form suitable for using to
    /// query the Destination service.
    ///
    /// # Returns
    /// - `DestinationServiceQuery::Inactive` if the authority is not suitable
    ///    for querying the Destination service, or the provided `client` was
    ///    `None`.
    /// - `DestinationServiceQuery::Active` if the authority is suitable for
    ///    querying the Destination service.
    fn query(&mut self, dst: &NameAddr, connect_or_reconnect: &str) -> Option<Query<T>>
    where
        T: GrpcService<BoxBody>,
    {
        trace!("DestinationServiceQuery {} {:?}", connect_or_reconnect, dst);
        if self.suffixes.iter().any(|s| s.contains(dst.name())) {
            let mut client = self.client.as_mut()?;
            let req = GetDestination {
                scheme: "k8s".into(),
                path: format!("{}", dst),
                context_token: self.context_token.clone(),
            };
            let mut svc = Destination::new(client.as_service());
            let response = svc.get(grpc::Request::new(req));
            Some(Remote::ConnectedOrConnecting {
                rx: Receiver::new(response),
            })
        } else {
            debug!("dst={} not in suffixes", dst.name());
            None
        }
    }
}

// ===== impl DestinationCache =====

impl<T> DestinationCache<T>
where
    T: GrpcService<BoxBody>,
{
    fn new() -> Self {
        Self {
            destinations: HashMap::new(),
            reconnects: VecDeque::new(),
        }
    }

    /// Ensures that `destinations` is updated to only maintain active resolutions.
    ///
    /// If there are no active resolutions for a destination, the destination is removed.
    fn retain_active(&mut self) {
        self.destinations
            .retain(|_, ref mut dst| dst.retain_active().is_active());
    }

    /// Tries to reconnect next watch stream. Returns true if reconnection started.
    fn poll_reconnect(&mut self, client: &mut NewQuery<T>) -> bool {
        while let Some(auth) = self.reconnects.pop_front() {
            if let Some(set) = self.destinations.get_mut(&auth) {
                set.reconnect(&auth, client);
                return true;
            } else {
                trace!("reconnect no longer needed: {:?}", auth);
            }
        }
        false
    }

    fn poll_destinations(&mut self) {
        for (auth, set) in &mut self.destinations {
            if set.poll_dst(auth) {
                self.reconnects.push_back(auth.clone())
            }
        }
    }
}

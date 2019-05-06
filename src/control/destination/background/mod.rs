use futures::{sync::mpsc, Async, Future, Poll, Stream};
use std::collections::{
    hash_map::{Entry, HashMap},
    VecDeque,
};
use tower_grpc::{self as grpc, generic::client::GrpcService, BoxBody};

use api::destination::client::Destination;
use api::destination::{GetDestination, Update as PbUpdate};

use super::ResolveRequest;
use control::remote_stream::{Receiver, Remote};
use dns;
use NameAddr;

mod destination_set;

use self::destination_set::DestinationSet;

type Query<T> = Remote<PbUpdate, T>;
type UpdateRx<T> = Receiver<PbUpdate, T>;

/// Satisfies resolutions as requested via `request_rx`.
///
/// As the `Background` is polled, if the client to the Destination service is
/// healthy, it reads requests from `request_rx`, determines how to resolve the
/// provided authority to a set of addresses, and ensures that resolution
/// updates are propagated to all requesters.
pub struct Background<T>
where
    T: GrpcService<BoxBody>,
{
    client: Client<T>,
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

/// Constructs new Destination service queries.
struct Client<T> {
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

            // purge inactive destination sets from the cache, and drive those
            // that remain
            self.dsts.retain_active().poll_destinations();

            if self.dsts.reconnects.is_empty() || !self.client.is_ready() {
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
            client: Client {
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
            if let Async::NotReady = self.client.poll_ready() {
                return Async::NotReady;
            }

            // handle any pending reconnects first
            if self.dsts.poll_reconnect(&mut self.client) {
                continue;
            }

            // check for any new watches
            match self.request_rx.poll() {
                Ok(Async::Ready(Some(resolve))) => {
                    trace!("Destination.Get {:?}", resolve.authority);

                    let client = &mut self.client;
                    let dsts = &mut self.dsts;

                    match dsts.destinations.entry(resolve.authority) {
                        Entry::Occupied(mut occ) => {
                            occ.get_mut().add_responder(resolve.responder);
                        }
                        Entry::Vacant(vac) => {
                            let set = DestinationSet::new(vac.key(), resolve.responder, client);
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

impl<T> Client<T>
where
    T: GrpcService<BoxBody>,
{
    /// Poll the underlying client service, returning its readiness.
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

    /// Returns `true` if the Destination service client was ready the last tiem
    /// `poll_ready` was called.
    fn is_ready(&self) -> bool {
        self.rpc_ready
    }

    /// Attepts to initiate a query to the Destination service if the given
    /// authority matches the client's set of search suffixes.
    ///
    /// # Returns
    /// - `None` if the authority is not suitable for querying the Destination
    //     service, or the underlying client service is `None`,
    /// - `Some(Query)` if the authority is suitable for querying the
    ///    Destination service.
    fn query(&mut self, dst: &NameAddr, connect_or_reconnect: &str) -> Option<Query<T>> {
        trace!("DestinationServiceQuery {} {:?}", connect_or_reconnect, dst);
        if self.suffixes.iter().any(|s| s.contains(dst.name())) {
            let client = self.client.as_mut()?;
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
    fn retain_active(&mut self) -> &mut Self {
        self.destinations
            .retain(|_, ref mut dst| dst.retain_active().is_active());
        self
    }

    /// Tries to reconnect next watch stream. Returns true if reconnection started.
    fn poll_reconnect(&mut self, client: &mut Client<T>) -> bool {
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

    /// Drives the destination sets in the cache, queueing any that need
    /// need to be reconnected.
    fn poll_destinations(&mut self) -> &mut Self {
        for (auth, set) in &mut self.destinations {
            // poll_dst returns `true` if the destination set should be
            // queued to be reconnected.
            if set.poll_dst(auth) {
                self.reconnects.push_back(auth.clone())
            }
        }
        self
    }
}

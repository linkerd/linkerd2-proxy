use std::{
    collections::{
        hash_map::{Entry, HashMap},
        VecDeque,
    },
    fmt,
    mem,
    time::Instant,
    sync::Arc,
};
use futures::{
    sync::mpsc,
    Async, Poll, Stream,
};
use tower_grpc::{self as grpc, Body, BoxBody};
use tower_http::HttpService;

use api::destination::client::Destination;
use api::destination::{
    GetDestination,
    Update as PbUpdate,
};

use super::{ResolveRequest, Update};
use app::config::Namespaces;
use control::{
    cache::Exists,
    remote_stream::{Receiver, Remote},
};
use dns;
use NameAddr;

mod destination_set;

use self::destination_set::DestinationSet;

type ActiveQuery<T> = Remote<PbUpdate, T>;
type UpdateRx<T> = Receiver<PbUpdate, T>;

/// Satisfies resolutions as requested via `request_rx`.
///
/// As the `Background` is polled with a client to Destination service, if the client to the
/// service is healthy, it reads requests from `request_rx`, determines how to resolve the
/// provided authority to a set of addresses, and ensures that resolution updates are
/// propagated to all requesters.
pub(super) struct Background<T>
where
    T: HttpService<BoxBody>,
    T::ResponseBody: Body,
{
    new_query: NewQuery,
    dns_resolver: dns::Resolver,
    dsts: DestinationCache<T>,
    /// The Destination.Get RPC client service.
    /// Each poll, records whether the rpc service was till ready.
    rpc_ready: bool,
    /// A receiver of new watch requests.
    request_rx: mpsc::UnboundedReceiver<ResolveRequest>,
}

/// Holds the currently active `DestinationSet`s and a list of any destinations
/// which require reconnects.
#[derive(Default)]
struct DestinationCache<T>
where
    T: HttpService<BoxBody>,
    T::ResponseBody: Body,
{
    destinations: HashMap<NameAddr, DestinationSet<T>>,
    /// A queue of authorities that need to be reconnected.
    reconnects: VecDeque<NameAddr>,
}

/// The configurationn necessary to create a new Destination service
/// query.
struct NewQuery {
    namespaces: Namespaces,
    suffixes: Vec<dns::Suffix>,
    /// Used for counting the number of currently-active queries.
    ///
    /// Each active query will hold a `Weak` reference back to this `Arc`, and
    /// `NewQuery` can use `Arc::weak_count` to count the number of queries
    /// that currently exist. When those queries are dropped, the weak count
    /// will go down accordingly.
    active_query_handle: Arc<()>,
    concurrency_limit: usize,
}

enum DestinationServiceQuery<T>
where
    T: HttpService<BoxBody>,
    T::ResponseBody: Body,
{
    Inactive,
    Active(ActiveQuery<T>),
    NoCapacity,
}

// ==== impl Background =====

impl<T> Background<T>
where
    T: HttpService<BoxBody>,
    T::ResponseBody: Body,
    T::Error: fmt::Debug,
{
    pub(super) fn new(
        request_rx: mpsc::UnboundedReceiver<ResolveRequest>,
        dns_resolver: dns::Resolver,
        namespaces: Namespaces,
        suffixes: Vec<dns::Suffix>,
        concurrency_limit: usize,
    ) -> Self {
        Self {
            new_query: NewQuery::new(namespaces, suffixes, concurrency_limit),
            dns_resolver,
            dsts: DestinationCache::new(),
            rpc_ready: false,
            request_rx,
        }
    }

   pub(super) fn poll_rpc(&mut self, client: &mut Option<T>) -> Poll<(), ()> {
        // This loop is make sure any streams that were found disconnected
        // in `poll_destinations` while the `rpc` service is ready should
        // be reconnected now, otherwise the task would just sleep...
        loop {
            if let Async::Ready(()) = self.poll_resolve_requests(client) {
                // request_rx has closed, meaning the main thread is terminating.
                return Ok(Async::Ready(()));
            }
            self.dsts.retain_active();
            self.poll_destinations();

            if self.dsts.reconnects.is_empty() || !self.rpc_ready {
                return Ok(Async::NotReady);
            }
        }
    }

    fn poll_resolve_requests(&mut self, client: &mut Option<T>) -> Async<()> {
        loop {
            if let Some(client) = client {
                // if rpc service isn't ready, not much we can do...
                match client.poll_ready() {
                    Ok(Async::Ready(())) => {
                        self.rpc_ready = true;
                    },
                    Ok(Async::NotReady) => {
                        self.rpc_ready = false;
                        return Async::NotReady;
                    },
                    Err(err) => {
                        warn!("Destination.Get poll_ready error: {:?}", err);
                        self.rpc_ready = false;
                        return Async::NotReady;
                    },
                }

                // handle any pending reconnects first
                if self.poll_reconnect(client) {
                    continue;
                }
            }

            // check for any new watches
            match self.request_rx.poll() {
                Ok(Async::Ready(Some(resolve))) => {
                    trace!("Destination.Get {:?}", resolve.authority);

                    let new_query = &self.new_query;
                    let dsts = &mut self.dsts;

                    // If the requested authority currently needs more
                    // query capacity to query the destination service, go
                    // ahead and try to free up capacity (by dropping any
                    // inactive DestinationSets).
                    if dsts.needs_query_for(&resolve.authority) {
                        trace!("--> no query capacity, try retain_active...", );
                        dsts.retain_active();
                    };

                    match dsts.destinations.entry(resolve.authority) {
                        Entry::Occupied(mut occ) => {
                            // we may already know of some addresses here, so push
                            // them onto the new watch first
                            match occ.get().addrs {
                                Exists::Yes(ref cache) => for (&addr, meta) in cache {
                                    let update = Update::Add(addr, meta.clone());
                                    resolve.responder.update_tx
                                        .unbounded_send(update)
                                        .expect("unbounded_send does not fail");
                                },
                                Exists::No | Exists::Unknown => (),
                            }

                            if occ.get().needs_query_capacity() {
                                trace!("--> {:?} wants to query Destination", occ.key());
                                let query = new_query
                                    .query_destination_service_if_relevant(
                                        client.as_mut(),
                                        occ.key(),
                                        "connect (previously at capacity)",
                                    );
                                occ.get_mut().query = query;
                            }

                            occ.get_mut().responders.push(resolve.responder);
                        },
                        Entry::Vacant(vac) => {
                            let query = new_query
                                .query_destination_service_if_relevant(
                                    client.as_mut(),
                                    vac.key(),
                                    "connect",
                                );
                            let mut set = DestinationSet {
                                addrs: Exists::Unknown,
                                query,
                                dns_query: None,
                                responders: vec![resolve.responder],
                            };
                            // If the authority is one for which the Destination service is never
                            // relevant (e.g. an absolute name that doesn't end in ".svc.$zone." in
                            // Kubernetes), or if we don't have a `client`, then immediately start
                            // polling DNS.
                            if !set.query.is_active() {
                                set.reset_dns_query(
                                    &self.dns_resolver,
                                    Instant::now(),
                                    vac.key(),
                                );
                            }
                            vac.insert(set);
                        },
                    }
                },
                Ok(Async::Ready(None)) => {
                    trace!("Discover tx is dropped, shutdown");
                    return Async::Ready(());
                },
                Ok(Async::NotReady) => return Async::NotReady,
                Err(_) => unreachable!("unbounded receiver doesn't error"),
            }
        }
    }

    /// Tries to reconnect next watch stream. Returns true if reconnection started.
    fn poll_reconnect(&mut self, client: &mut T) -> bool {
        debug_assert!(self.rpc_ready);

        while let Some(auth) = self.dsts.reconnects.pop_front() {
            if let Some(set) = self.dsts.destinations.get_mut(&auth) {
                set.query = self.new_query
                    .query_destination_service_if_relevant(
                        Some(client),
                        &auth,
                        "reconnect",
                    );
                return !set.needs_query_capacity();
            } else {
                trace!("reconnect no longer needed: {:?}", auth);
            }
        }
        false
    }

    fn poll_destinations(&mut self) {
        for (auth, set) in &mut self.dsts.destinations {
            // Query the Destination service first.
            let (new_query, found_by_destination_service) = match set.query.take() {
                DestinationServiceQuery::Active(Remote::ConnectedOrConnecting { rx }) => {
                    let (new_query, found_by_destination_service) =
                        set.poll_destination_service(
                            auth,
                            rx,
                            self.new_query.tls_controller_ns(),
                        );
                    if let Remote::NeedsReconnect = new_query {
                        set.reset_on_next_modification();
                        self.dsts.reconnects.push_back(auth.clone());
                    }
                    (new_query.into(), found_by_destination_service)
                },
                query => (query, Exists::Unknown),
            };
            set.query = new_query;

            // Any active response from the Destination service cancels the DNS query except for a
            // positive assertion that the service doesn't exist.
            //
            // Any disconnection from the Destination service has no effect on the DNS query; we
            // assume that if we were querying DNS before, we should continue to do so, and if we
            // weren't querying DNS then we shouldn't start now. In particular, temporary
            // disruptions of connectivity to the Destination service do not cause a fallback to
            // DNS.
            match found_by_destination_service {
                Exists::Yes(()) => {
                    // Stop polling DNS on any active update from the Destination service.
                    set.dns_query = None;
                },
                Exists::No => {
                    // Fall back to DNS.
                    set.reset_dns_query(&self.dns_resolver, Instant::now(), auth);
                },
                Exists::Unknown => (), // No change from Destination service's perspective.
            }

            // Poll DNS after polling the Destination service. This may reset the DNS query but it
            // won't affect the Destination Service query.
            set.poll_dns(&self.dns_resolver, auth);
        }
    }

}

// ===== impl NewQuery =====

impl NewQuery {

    fn new(namespaces: Namespaces, suffixes: Vec<dns::Suffix>, concurrency_limit: usize) -> Self {
        Self {
            namespaces,
            suffixes,
            concurrency_limit,
            active_query_handle: Arc::new(()),
        }
    }

    /// Returns true if there is currently capacity for additional
    /// Destination service queries.
    fn has_more_queries(&self) -> bool {
        Arc::weak_count(&self.active_query_handle) < self.concurrency_limit
    }

    /// Attepts to initiate a query `query` to the Destination service
    /// if the given authority's host is of a form suitable for using to
    /// query the Destination service.
    ///
    /// # Returns
    ///
    /// - `DestinationServiceQuery::NoCapacity` if the given authority _should_
    ///   query the Destination service, but the query limit has already been
    ///   reached. In this case, this function should be called again when
    ///   there is capacity for additional queries.
    /// - `DestinationServiceQuery::Inactive` if the authority is not suitable
    ///    for querying the Destination service, or the provided `client` was
    ///    `None`.
    /// - `DestinationServiceQuery::Active` if the authority is suitable for
    ///    querying the Destination service and there is sufficient capacity to
    ///    initiate a new query.
    fn query_destination_service_if_relevant<T>(
        &self,
        client: Option<&mut T>,
        dst: &NameAddr,
        connect_or_reconnect: &str,
    ) -> DestinationServiceQuery<T>
    where
        T: HttpService<BoxBody>,
        T::ResponseBody: Body,
        T::Error: fmt::Debug,
    {
        trace!("DestinationServiceQuery {} {:?}", connect_or_reconnect, dst);
        if !self.suffixes.iter().any(|s| s.contains(dst.name())) {
            debug!("dst={} not in suffixes", dst.name());
            return DestinationServiceQuery::Inactive;
        }
        match client {
            // If we were able to normalize the authority (indicating we should
            // query the Destination service), but we're out of queries, return
            // None and set the "needs query" flag.
            Some(_) if !self.has_more_queries() => {
                warn!(
                    "Can't query Destination service for {:?}, maximum \
                     number of queries ({}) reached.",
                    dst,
                    self.concurrency_limit,
                );
                DestinationServiceQuery::NoCapacity
            },
            // We should query the Destination service and there is sufficient
            // query capacity, so we're good to go!
            Some(client) => {
                let req = GetDestination {
                    scheme: "k8s".into(),
                    path: format!("{}", dst),
                };
                let mut svc = Destination::new(client.lift_ref());
                let response = svc.get(grpc::Request::new(req));
                let active = Arc::downgrade(&self.active_query_handle);
                let query = Remote::ConnectedOrConnecting {
                    rx: Receiver::new(response, active),
                };
                DestinationServiceQuery::Active(query)
            },
            // This authority should not query the Destination service. Return
            // None, but set the "needs query" flag to false.
            _ => DestinationServiceQuery::Inactive
        }
    }

    fn tls_controller_ns(&self) -> Option<&str> {
        self.namespaces.tls_controller.as_ref().map(String::as_ref)
    }
}

// ===== impl DestinationCache =====

impl<T> DestinationCache<T>
where
    T: HttpService<BoxBody>,
    T::ResponseBody: Body,
    T::Error: fmt::Debug,
{

    fn new() -> Self {
        Self {
            destinations: HashMap::new(),
            reconnects: VecDeque::new(),
        }
    }

    /// Returns true if `auth` is currently known to need a Destination
    /// service query, but was unable to query previously due to the query
    /// limit being reached.
    fn needs_query_for(&self, auth: &NameAddr) -> bool {
        self.destinations
            .get(auth)
            .map(|dst| dst.needs_query_capacity())
            .unwrap_or(false)
    }

    /// Ensures that `destinations` is updated to only maintain active resolutions.
    ///
    /// If there are no active resolutions for a destination, the destination is removed.
    fn retain_active(&mut self) {
        self.destinations.retain(|_, ref mut dst| {
            dst.responders.retain(|r| r.is_active());
            dst.responders.len() > 0
        });
    }
}

// ===== impl DestinationServiceQuery =====

impl<T> DestinationServiceQuery<T>
where
    T: HttpService<BoxBody>,
    T::ResponseBody: Body,
{

    pub fn is_active(&self) -> bool {
        match self {
            DestinationServiceQuery::Active(_) => true,
            _ => false,
        }
    }

    /// Returns `true` if the authority that created this query _should_ query
    /// the Destination service, but was unable to due to insufficient capaacity.
    pub fn needs_query_capacity(&self) -> bool {
        match self {
            DestinationServiceQuery::NoCapacity => true,
            _ => false,
        }
    }

    pub fn take(&mut self) -> Self {
         mem::replace(self, DestinationServiceQuery::Inactive)
    }

}

impl<T> From<ActiveQuery<T>> for DestinationServiceQuery<T>
where
    T: HttpService<BoxBody>,
    T::ResponseBody: Body,
{
    fn from(active: ActiveQuery<T>) -> Self {
        DestinationServiceQuery::Active(active)
    }
}

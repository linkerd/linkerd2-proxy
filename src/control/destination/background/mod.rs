use std::{
    collections::{
        hash_map::{Entry, HashMap},
        VecDeque,
    },
    fmt,
    time::{Instant, Duration},
};
use futures::{
    future,
    sync::mpsc,
    Async, Future, Poll, Stream,
};
use tower_grpc as grpc;
use tower_h2::{BoxBody, HttpService, RecvBody};

use linkerd2_proxy_api::destination::client::Destination;
use linkerd2_proxy_api::destination::{
    GetDestination,
    Update as PbUpdate,
};

use super::{ResolveRequest, Update};
use config::Namespaces;
use control::{
    cache::Exists,
    fully_qualified_authority::FullyQualifiedAuthority,
    remote_stream::{Receiver, Remote},
};
use dns;
use transport::{tls, DnsNameAndPort, HostAndPort};
use conditional::Conditional;
use watch_service::WatchService;
use futures_watch::Watch;

mod client;
mod destination_set;

use self::{
    client::BindClient,
    destination_set::DestinationSet,
};

type DestinationServiceQuery<T> = Remote<PbUpdate, T>;
type UpdateRx<T> = Receiver<PbUpdate, T>;

/// Satisfies resolutions as requested via `request_rx`.
///
/// As the `Background` is polled with a client to Destination service, if the client to the
/// service is healthy, it reads requests from `request_rx`, determines how to resolve the
/// provided authority to a set of addresses, and ensures that resolution updates are
/// propagated to all requesters.
struct Background<T: HttpService<ResponseBody = RecvBody>> {
    dns_resolver: dns::Resolver,
    namespaces: Namespaces,
    destinations: HashMap<DnsNameAndPort, DestinationSet<T>>,
    /// A queue of authorities that need to be reconnected.
    reconnects: VecDeque<DnsNameAndPort>,
    /// The Destination.Get RPC client service.
    /// Each poll, records whether the rpc service was till ready.
    rpc_ready: bool,
    /// A receiver of new watch requests.
    request_rx: mpsc::UnboundedReceiver<ResolveRequest>,
}

/// Returns a new discovery background task.
pub(super) fn task(
    request_rx: mpsc::UnboundedReceiver<ResolveRequest>,
    dns_resolver: dns::Resolver,
    namespaces: Namespaces,
    host_and_port: Option<HostAndPort>,
    controller_tls: tls::ConditionalConnectionConfig<tls::ClientConfigWatch>,
    control_backoff_delay: Duration,
) -> impl Future<Item = (), Error = ()>
{
    // Build up the Controller Client Stack
    let mut client = host_and_port.map(|host_and_port| {
        let (identity, watch) = match controller_tls {
            Conditional::Some(cfg) =>
                (Conditional::Some(cfg.server_identity), cfg.config),
            Conditional::None(reason) => {
                // If there's no connection config, then construct a new
                // `Watch` that never updates to construct the `WatchService`.
                // We do this here rather than calling `ClientConfig::no_tls`
                // in order to propagate the reason for no TLS to the watch.
                let (watch, _) = Watch::new(Conditional::None(reason));
                (Conditional::None(reason), watch)
            },
        };
        let bind_client = BindClient::new(
            identity,
            &dns_resolver,
            host_and_port,
            control_backoff_delay,
        );
        WatchService::new(watch, bind_client)
    });

    let mut disco = Background::new(
        request_rx,
        dns_resolver,
        namespaces,
    );

    future::poll_fn(move || {
        disco.poll_rpc(&mut client)
    })
}

// ==== impl Background =====

impl<T> Background<T>
where
    T: HttpService<RequestBody = BoxBody, ResponseBody = RecvBody>,
    T::Error: fmt::Debug,
{
    fn new(
        request_rx: mpsc::UnboundedReceiver<ResolveRequest>,
        dns_resolver: dns::Resolver,
        namespaces: Namespaces,
    ) -> Self {
        Self {
            dns_resolver,
            namespaces,
            destinations: HashMap::new(),
            reconnects: VecDeque::new(),
            rpc_ready: false,
            request_rx,
        }
    }

   fn poll_rpc(&mut self, client: &mut Option<T>) -> Poll<(), ()> {
        // This loop is make sure any streams that were found disconnected
        // in `poll_destinations` while the `rpc` service is ready should
        // be reconnected now, otherwise the task would just sleep...
        loop {
            if let Async::Ready(()) = self.poll_resolve_requests(client) {
                // request_rx has closed, meaning the main thread is terminating.
                return Ok(Async::Ready(()));
            }
            self.retain_active_destinations();
            self.poll_destinations();

            if self.reconnects.is_empty() || !self.rpc_ready {
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
                    match self.destinations.entry(resolve.authority) {
                        Entry::Occupied(mut occ) => {
                            let set = occ.get_mut();
                            // we may already know of some addresses here, so push
                            // them onto the new watch first
                            match set.addrs {
                                Exists::Yes(ref cache) => for (&addr, meta) in cache {
                                    let update = Update::Bind(addr, meta.clone());
                                    resolve.responder.update_tx
                                        .unbounded_send(update)
                                        .expect("unbounded_send does not fail");
                                },
                                Exists::No | Exists::Unknown => (),
                            }
                            set.responders.push(resolve.responder);
                        },
                        Entry::Vacant(vac) => {
                            let pod_namespace = &self.namespaces.pod;
                            let query = client.as_mut().and_then(|client| {
                                Self::query_destination_service_if_relevant(
                                    pod_namespace,
                                    client,
                                    vac.key(),
                                    "connect",
                                )
                            });
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
                            if set.query.is_none() {
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

        while let Some(auth) = self.reconnects.pop_front() {
            if let Some(set) = self.destinations.get_mut(&auth) {
                set.query = Self::query_destination_service_if_relevant(
                    &self.namespaces.pod,
                    client,
                    &auth,
                    "reconnect",
                );
                return true;
            } else {
                trace!("reconnect no longer needed: {:?}", auth);
            }
        }
        false
    }

    /// Ensures that `destinations` is updated to only maintain active resolutions.
    ///
    /// If there are no active resolutions for a destination, the destination is removed.
    fn retain_active_destinations(&mut self) {
        self.destinations.retain(|_, ref mut dst| {
            dst.responders.retain(|r| r.is_active());
            dst.responders.len() > 0
        })
    }

    fn poll_destinations(&mut self) {
        for (auth, set) in &mut self.destinations {
            // Query the Destination service first.
            let (new_query, found_by_destination_service) = match set.query.take() {
                Some(Remote::ConnectedOrConnecting { rx }) => {
                    let (new_query, found_by_destination_service) =
                        set.poll_destination_service(
                            auth, rx, self.namespaces.tls_controller.as_ref().map(|s| s.as_ref()));
                    if let Remote::NeedsReconnect = new_query {
                        set.reset_on_next_modification();
                        self.reconnects.push_back(auth.clone());
                    }
                    (Some(new_query), found_by_destination_service)
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

    /// Initiates a query `query` to the Destination service and returns it as
    /// `Some(query)` if the given authority's host is of a form suitable for using to
    /// query the Destination service. Otherwise, returns `None`.
    fn query_destination_service_if_relevant(
        default_destination_namespace: &str,
        client: &mut T,
        auth: &DnsNameAndPort,
        connect_or_reconnect: &str,
    ) -> Option<DestinationServiceQuery<T>> {
        trace!(
            "DestinationServiceQuery {} {:?}",
            connect_or_reconnect,
            auth
        );
        FullyQualifiedAuthority::normalize(auth, default_destination_namespace).map(|auth| {
            let req = GetDestination {
                scheme: "k8s".into(),
                path: auth.without_trailing_dot().to_owned(),
            };
            let mut svc = Destination::new(client.lift_ref());
            let response = svc.get(grpc::Request::new(req));
            Remote::ConnectedOrConnecting {
                rx: Receiver::new(response),
            }
        })
    }
}

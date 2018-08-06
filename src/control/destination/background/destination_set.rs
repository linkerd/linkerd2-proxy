use std::{
    collections::HashMap,
    fmt,
    iter::IntoIterator,
    net::SocketAddr,
    sync::Arc,
    time::{Instant, Duration},
};

use futures::{Async, Future, Stream,};
use tower_h2::{BoxBody, HttpService, RecvBody};

use linkerd2_proxy_api::{
    destination::{
        update::Update as PbUpdate2,
        WeightedAddr,
    },
    net::TcpAddress,
};

use super::super::{Metadata, Responder, Update};
use control::{
    cache::{Cache, CacheChange, Exists},
    remote_stream::Remote,
};
use dns::{self, IpAddrListFuture};
use telemetry::metrics::DstLabels;
use transport::{tls, DnsNameAndPort};
use conditional::Conditional;

use super::{ActiveQuery, DestinationServiceQuery, UpdateRx};

/// Holds the state of a single resolution.
pub(super) struct DestinationSet<T: HttpService<ResponseBody = RecvBody>> {
    auth: Arc<DnsNameAndPort>,
    pub addrs: Exists<Cache<SocketAddr, Metadata>>,
    pub query: DestinationServiceQuery<T>,
    dns_query: Option<IpAddrListFuture>,
    responders: Vec<Responder>,
}

// ===== impl DestinationSet =====

impl<T> DestinationSet<T>
where
    T: HttpService<RequestBody = BoxBody, ResponseBody = RecvBody>,
    T::Error: fmt::Debug,
{
    pub(super) fn new(
        auth: Arc<DnsNameAndPort>,
        responder: Responder,
        new_query: &super::NewQuery,
        dns_resolver: &dns::Resolver,
        client: Option<&mut T>,
    ) -> Self {
        let query = new_query
            .query_destination_service_if_relevant(
                client,
                &auth,
                "connect",
            );

        let mut set = DestinationSet {
            auth,
            addrs: Exists::Unknown,
            query,
            dns_query: None,
            responders: vec![responder],
        };


        // If the authority is one for which the Destination service is never
        // relevant (e.g. an absolute name that doesn't end in ".svc.$zone." in
        // Kubernetes), or if we don't have a `client`, then immediately start
        // polling DNS.
        if !set.query.is_active() {
            set.reset_dns_query(dns_resolver, Instant::now());
        }

        set
    }

    pub(super) fn reconnect_destination_query(
        &mut self,
        new_query: &super::NewQuery,
        client: Option<&mut T>,
    ) -> bool {
        self.query = new_query
            .query_destination_service_if_relevant(
                client,
                &self.auth,
                "reconnect",
            );
        self.query.is_active()
    }

    pub(super) fn reset_dns_query(
        &mut self,
        dns_resolver: &dns::Resolver,
        deadline: Instant,
    ) {
        trace!(
            "resetting DNS query for {} at {:?}",
            self.auth.host,
            deadline
        );
        self.reset_on_next_modification();
        self.dns_query = Some(dns_resolver.resolve_all_ips(deadline, &self.auth.host));
    }

    pub(super) fn end_dns_query(&mut self) {
        self.dns_query = None;
    }

    pub(super) fn add_responder(&mut self, responder: Responder) {
        self.responders.push(responder)
    }

    /// Drops any responders which have become inactive.
    ///
    /// # Returns
    /// `true` if this `DestinationSet` is still active after cleaning up any
    /// inactive responders, `false` if it is inactive.
    pub(super) fn retain_active_responders(&mut self) -> bool {
        self.responders.retain(Responder::is_active);
        self.responders.len() > 0
    }

    // Processes Destination service updates from `request_rx`, returning the new query
    // and an indication of any *change* to whether the service exists as far as the
    // Destination service is concerned, where `Exists::Unknown` is to be interpreted as
    // "no change in existence" instead of "unknown".
    pub(super) fn poll_destination_service(
        &mut self,
        mut rx: UpdateRx<T>,
        tls_controller_namespace: Option<&str>,
    ) -> (ActiveQuery<T>, Exists<()>) {
        let mut exists = Exists::Unknown;

        loop {
            match rx.poll() {
                Ok(Async::Ready(Some(update))) => match update.update {
                    Some(PbUpdate2::Add(a_set)) => {
                        let set_labels = a_set.metric_labels;
                        let addrs = a_set
                            .addrs
                            .into_iter()
                            .filter_map(|pb|
                                pb_to_addr_meta(pb, &set_labels, tls_controller_namespace));
                        self.add(addrs)
                    },
                    Some(PbUpdate2::Remove(r_set)) => {
                        exists = Exists::Yes(());
                        self.remove(
                            r_set
                                .addrs
                                .iter()
                                .filter_map(|addr| pb_to_sock_addr(addr.clone())),
                        );
                    },
                    Some(PbUpdate2::NoEndpoints(ref no_endpoints)) if no_endpoints.exists => {
                        exists = Exists::Yes(());
                        self.no_endpoints(no_endpoints.exists);
                    },
                    Some(PbUpdate2::NoEndpoints(no_endpoints)) => {
                        debug_assert!(!no_endpoints.exists);
                        exists = Exists::No;
                    },
                    None => (),
                },
                Ok(Async::Ready(None)) => {
                    trace!(
                        "Destination.Get stream ended for {:?}, must reconnect",
                        self.auth
                    );
                    return (Remote::NeedsReconnect.into(), exists);
                },
                Ok(Async::NotReady) => {
                    return (Remote::ConnectedOrConnecting { rx }.into(), exists);
                },
                Err(err) => {
                    warn!("Destination.Get stream errored for {:?}: {:?}", self.auth, err);
                    return (Remote::NeedsReconnect.into(), exists);
                },
            };
        }
    }

    pub(super) fn poll_dns(&mut self, dns_resolver: &dns::Resolver) {
        // Duration to wait before polling DNS again after an error
        // (or a NXDOMAIN response with no TTL).
        const DNS_ERROR_TTL: Duration = Duration::from_secs(5);

        trace!("checking DNS for {:?}", self.auth);
        while let Some(mut query) = self.dns_query.take() {
            trace!("polling DNS for {:?}", self.auth);
            let deadline = match query.poll() {
                Ok(Async::NotReady) => {
                    trace!("DNS query not ready {:?}", self.auth);
                    self.dns_query = Some(query);
                    return;
                },
                Ok(Async::Ready(dns::Response::Exists(ips))) => {
                    trace!(
                        "positive result of DNS query for {:?}: {:?}",
                        self.auth,
                        ips
                    );
                    let port = self.auth.port;
                    self.add(
                        ips.iter().map(|ip| {
                            (
                                SocketAddr::from((ip, port)),
                                Metadata::no_metadata(),
                            )
                        }),
                    );

                    // Poll again after the deadline on the DNS response.
                    ips.valid_until()
                },
                Ok(Async::Ready(dns::Response::DoesNotExist { retry_after })) => {
                    trace!(
                        "negative result (NXDOMAIN) of DNS query for {:?}",
                        self.auth
                    );
                    self.no_endpoints(false);
                    // Poll again after the deadline on the DNS response, if
                    // there is one.
                    retry_after.unwrap_or_else(|| Instant::now() + DNS_ERROR_TTL)
                },
                Err(e) => {
                    // Do nothing so that the most recent non-error response is used until a
                    // non-error response is received
                    trace!("DNS resolution failed for {}: {}", self.auth.host, e);

                    // Poll again after the default wait time.
                    Instant::now() + DNS_ERROR_TTL
                },
            };
            self.reset_dns_query(dns_resolver, deadline)
        }
    }
}

impl<T: HttpService<ResponseBody = RecvBody>> DestinationSet<T> {

    /// Returns `true` if the authority that created this query _should_ query
    /// the Destination service, but was unable to due to insufficient capaacity.
    pub(super) fn needs_query_capacity(&self) -> bool {
        self.query.needs_query_capacity()
    }

    pub(super) fn reset_on_next_modification(&mut self) {
        match self.addrs {
            Exists::Yes(ref mut cache) => {
                cache.set_reset_on_next_modification();
            },
            Exists::No | Exists::Unknown => (),
        }
    }

    fn add<A>(&mut self, addrs_to_add: A)
    where
        A: Iterator<Item = (SocketAddr, Metadata)>,
    {
        let mut cache = match self.addrs.take() {
            Exists::Yes(mut cache) => cache,
            Exists::Unknown | Exists::No => Cache::new(),
        };
        cache.update_union(addrs_to_add, &mut |change| {
            Self::on_change(&mut self.responders, &self.auth, change)
        });
        self.addrs = Exists::Yes(cache);
    }

    fn remove<A>(&mut self, addrs_to_remove: A)
    where
        A: Iterator<Item = SocketAddr>,
    {
        let cache = match self.addrs.take() {
            Exists::Yes(mut cache) => {
                cache.remove(addrs_to_remove, &mut |change| {
                    Self::on_change(&mut self.responders, &self.auth, change)
                });
                cache
            },
            Exists::Unknown | Exists::No => Cache::new(),
        };
        self.addrs = Exists::Yes(cache);
    }

    fn no_endpoints(&mut self, exists: bool) {
        trace!(
            "no endpoints for {:?} that is known to {}",
            &self.auth,
            if exists { "exist" } else { "not exist" }
        );
        match self.addrs.take() {
            Exists::Yes(mut cache) => {
                cache.clear(&mut |change| {
                    Self::on_change(&mut self.responders, &self.auth, change)
                });
            },
            Exists::Unknown | Exists::No => (),
        };
        self.addrs = if exists {
            Exists::Yes(Cache::new())
        } else {
            Exists::No
        };
    }

    fn on_change(
        responders: &mut Vec<Responder>,
        authority_for_logging: &DnsNameAndPort,
        change: CacheChange<SocketAddr, Metadata>,
    ) {
        let (update_str, update, addr) = match change {
            CacheChange::Insertion { key, value } => {
                ("insert", Update::Bind(key, value.clone()), key)
            },
            CacheChange::Removal { key } => ("remove", Update::Remove(key), key),
            CacheChange::Modification { key, new_value } => (
                "change metadata for",
                Update::Bind(key, new_value.clone()),
                key,
            ),
        };
        trace!("{} {:?} for {:?}", update_str, addr, authority_for_logging);
        // retain is used to drop any senders that are dead
        responders.retain(|r| {
            let sent = r.update_tx.unbounded_send(update.clone());
            sent.is_ok()
        });
    }
}


/// Construct a new labeled `SocketAddr `from a protobuf `WeightedAddr`.
fn pb_to_addr_meta(
    pb: WeightedAddr,
    set_labels: &HashMap<String, String>,
    tls_controller_namespace: Option<&str>,
) -> Option<(SocketAddr, Metadata)> {
    let addr = pb.addr.and_then(pb_to_sock_addr)?;

    let mut labels = set_labels.iter()
        .chain(pb.metric_labels.iter())
        .collect::<Vec<_>>();
    labels.sort_by(|(k0, _), (k1, _)| k0.cmp(k1));

    let mut tls_identity =
        Conditional::None(tls::ReasonForNoIdentity::NotProvidedByServiceDiscovery);
    if let Some(pb) = pb.tls_identity {
        match tls::Identity::maybe_from_protobuf(tls_controller_namespace, pb) {
            Ok(Some(identity)) => {
                tls_identity = Conditional::Some(identity);
            },
            Ok(None) => (),
            Err(e) => {
                error!("Failed to parse TLS identity: {:?}", e);
                // XXX: Wallpaper over the error and keep going without TLS.
                // TODO: Hard fail here once the TLS infrastructure has been
                // validated.
            },
        }
    };

    let meta = Metadata::new(DstLabels::new(labels.into_iter()), tls_identity);
    Some((addr, meta))
}

fn pb_to_sock_addr(pb: TcpAddress) -> Option<SocketAddr> {
    use linkerd2_proxy_api::net::ip_address::Ip;
    use std::net::{Ipv4Addr, Ipv6Addr};
    /*
    current structure is:
    TcpAddress {
        ip: Option<IpAddress {
            ip: Option<enum Ip {
                Ipv4(u32),
                Ipv6(IPv6 {
                    first: u64,
                    last: u64,
                }),
            }>,
        }>,
        port: u32,
    }
    */
    match pb.ip {
        Some(ip) => match ip.ip {
            Some(Ip::Ipv4(octets)) => {
                let ipv4 = Ipv4Addr::from(octets);
                Some(SocketAddr::from((ipv4, pb.port as u16)))
            },
            Some(Ip::Ipv6(v6)) => {
                let octets = [
                    (v6.first >> 56) as u8,
                    (v6.first >> 48) as u8,
                    (v6.first >> 40) as u8,
                    (v6.first >> 32) as u8,
                    (v6.first >> 24) as u8,
                    (v6.first >> 16) as u8,
                    (v6.first >> 8) as u8,
                    v6.first as u8,
                    (v6.last >> 56) as u8,
                    (v6.last >> 48) as u8,
                    (v6.last >> 40) as u8,
                    (v6.last >> 32) as u8,
                    (v6.last >> 24) as u8,
                    (v6.last >> 16) as u8,
                    (v6.last >> 8) as u8,
                    v6.last as u8,
                ];
                let ipv6 = Ipv6Addr::from(octets);
                Some(SocketAddr::from((ipv6, pb.port as u16)))
            },
            None => None,
        },
        None => None,
    }
}

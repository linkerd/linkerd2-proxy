use indexmap::IndexMap;
use std::{
    collections::HashMap,
    fmt,
    iter::IntoIterator,
    net::SocketAddr,
    time::{Instant, Duration},
};

use futures::{Async, Future, Stream,};
use tower_http::HttpService;
use tower_grpc::{Body, BoxBody};

use api::{
    destination::{
        protocol_hint::Protocol,
        update::Update as PbUpdate2,
        WeightedAddr,
    },
    net::TcpAddress,
};

use control::{
    cache::{Cache, CacheChange, Exists},
    destination::{Metadata, ProtocolHint, Responder, Update},
    remote_stream::Remote,
};
use dns::{self, IpAddrListFuture};
use transport::tls;
use {Conditional, NameAddr};

use super::{ActiveQuery, DestinationServiceQuery, UpdateRx};

/// Holds the state of a single resolution.
pub(super) struct DestinationSet<T>
where
    T: HttpService<BoxBody>,
    T::ResponseBody: Body,
{
    pub addrs: Exists<Cache<SocketAddr, Metadata>>,
    pub query: DestinationServiceQuery<T>,
    pub dns_query: Option<IpAddrListFuture>,
    pub responders: Vec<Responder>,
}

// ===== impl DestinationSet =====

impl<T> DestinationSet<T>
where
    T: HttpService<BoxBody>,
    T::ResponseBody: Body,
    T::Error: fmt::Debug,
{
    pub(super) fn reset_dns_query(
        &mut self,
        dns_resolver: &dns::Resolver,
        deadline: Instant,
        authority: &NameAddr,
    ) {
        trace!(
            "resetting DNS query for {} at {:?}",
            authority.name(),
            deadline
        );
        self.reset_on_next_modification();
        self.dns_query = Some(dns_resolver.resolve_all_ips(deadline, authority.name()));
    }

    // Processes Destination service updates from `request_rx`, returning the new query
    // and an indication of any *change* to whether the service exists as far as the
    // Destination service is concerned, where `Exists::Unknown` is to be interpreted as
    // "no change in existence" instead of "unknown".
    pub(super) fn poll_destination_service(
        &mut self,
        auth: &NameAddr,
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
                        self.add(auth, addrs)
                    },
                    Some(PbUpdate2::Remove(r_set)) => {
                        exists = Exists::Yes(());
                        self.remove(
                            auth,
                            r_set
                                .addrs
                                .iter()
                                .filter_map(|addr| pb_to_sock_addr(addr.clone())),
                        );
                    },
                    Some(PbUpdate2::NoEndpoints(ref no_endpoints)) if no_endpoints.exists => {
                        exists = Exists::Yes(());
                        self.no_endpoints(auth, no_endpoints.exists);
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
                        auth
                    );
                    return (Remote::NeedsReconnect.into(), exists);
                },
                Ok(Async::NotReady) => {
                    return (Remote::ConnectedOrConnecting { rx }.into(), exists);
                },
                Err(err) => {
                    warn!("Destination.Get stream errored for {:?}: {:?}", auth, err);
                    return (Remote::NeedsReconnect.into(), exists);
                },
            };
        }
    }

    pub(super) fn poll_dns(&mut self, dns_resolver: &dns::Resolver, authority: &NameAddr) {
        // Duration to wait before polling DNS again after an error
        // (or a NXDOMAIN response with no TTL).
        const DNS_ERROR_TTL: Duration = Duration::from_secs(5);

        trace!("checking DNS for {:?}", authority);
        while let Some(mut query) = self.dns_query.take() {
            trace!("polling DNS for {:?}", authority);
            let deadline = match query.poll() {
                Ok(Async::NotReady) => {
                    trace!("DNS query not ready {:?}", authority);
                    self.dns_query = Some(query);
                    return;
                },
                Ok(Async::Ready(dns::Response::Exists(ips))) => {
                    trace!(
                        "positive result of DNS query for {:?}: {:?}",
                        authority,
                        ips
                    );
                    self.add(
                        authority,
                        ips.iter().map(|ip| {
                            (
                                SocketAddr::from((ip, authority.port())),
                                Metadata::none(tls::ReasonForNoIdentity::NotProvidedByServiceDiscovery),
                            )
                        }),
                    );

                    // Poll again after the deadline on the DNS response.
                    ips.valid_until()
                },
                Ok(Async::Ready(dns::Response::DoesNotExist { retry_after })) => {
                    trace!(
                        "negative result (NXDOMAIN) of DNS query for {:?}",
                        authority
                    );
                    self.no_endpoints(authority, false);
                    // Poll again after the deadline on the DNS response, if
                    // there is one.
                    retry_after.unwrap_or_else(|| Instant::now() + DNS_ERROR_TTL)
                },
                Err(e) => {
                    // Do nothing so that the most recent non-error response is used until a
                    // non-error response is received
                    trace!("DNS resolution failed for {}: {}", authority.name(), e);

                    // Poll again after the default wait time.
                    Instant::now() + DNS_ERROR_TTL
                },
            };
            self.reset_dns_query(dns_resolver, deadline, &authority)
        }
    }
}

impl<T> DestinationSet<T>
where
    T: HttpService<BoxBody>,
    T::ResponseBody: Body,
{
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

    fn add<A>(&mut self, authority_for_logging: &NameAddr, addrs_to_add: A)
    where
        A: Iterator<Item = (SocketAddr, Metadata)>,
    {
        let mut cache = match self.addrs.take() {
            Exists::Yes(mut cache) => cache,
            Exists::Unknown | Exists::No => Cache::new(),
        };
        cache.update_union(addrs_to_add, &mut |change| {
            Self::on_change(&mut self.responders, authority_for_logging, change)
        });
        self.addrs = Exists::Yes(cache);
    }

    fn remove<A>(&mut self, authority_for_logging: &NameAddr, addrs_to_remove: A)
    where
        A: Iterator<Item = SocketAddr>,
    {
        let cache = match self.addrs.take() {
            Exists::Yes(mut cache) => {
                cache.remove(addrs_to_remove, &mut |change| {
                    Self::on_change(&mut self.responders, authority_for_logging, change)
                });
                cache
            },
            Exists::Unknown | Exists::No => Cache::new(),
        };
        self.addrs = Exists::Yes(cache);
    }

    fn no_endpoints(&mut self, authority_for_logging: &NameAddr, exists: bool) {
        trace!(
            "no endpoints for {:?} that is known to {}",
            authority_for_logging,
            if exists { "exist" } else { "not exist" }
        );
        match self.addrs.take() {
            Exists::Yes(mut cache) => {
                cache.clear(&mut |change| {
                    Self::on_change(&mut self.responders, authority_for_logging, change)
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
        authority_for_logging: &NameAddr,
        change: CacheChange<SocketAddr, Metadata>,
    ) {
        let (update_str, update, addr) = match change {
            CacheChange::Insertion { key, value } => {
                ("insert", Update::Add(key, value.clone()), key)
            },
            CacheChange::Removal { key } => ("remove", Update::Remove(key), key),
            CacheChange::Modification { key, new_value } => (
                "change metadata for",
                Update::Add(key, new_value.clone()),
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

    let meta = {
        let mut t = set_labels.iter()
            .chain(pb.metric_labels.iter())
            .collect::<Vec<(&String, &String)>>();
        t.sort_by(|(k0, _), (k1, _)| k0.cmp(k1));

        let mut m = IndexMap::with_capacity(t.len());
        for (k, v) in t.into_iter() {
            m.insert(k.clone(), v.clone());
        }

        m
    };

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

    let mut proto_hint = ProtocolHint::Unknown;

    if let Some(hint) = pb.protocol_hint {
        if let Some(proto) = hint.protocol {
            match proto {
                Protocol::H2(..) => {
                    proto_hint = ProtocolHint::Http2;
                }
            }
        }
    }

    let meta = Metadata::new(meta, proto_hint, tls_identity);
    Some((addr, meta))
}

fn pb_to_sock_addr(pb: TcpAddress) -> Option<SocketAddr> {
    use api::net::ip_address::Ip;
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

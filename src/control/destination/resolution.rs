use futures::{future::Future, sync::mpsc, Async, Poll, Stream};
use indexmap::{IndexMap, IndexSet};
use std::{
    collections::{HashMap, VecDeque},
    fmt,
    net::SocketAddr,
};

use tokio;
use tower_grpc::{self as grpc, generic::client::GrpcService, Body, BoxBody};

use api::{
    destination::{
        client::Destination, protocol_hint::Protocol, update::Update as PbUpdate2, GetDestination,
        TlsIdentity, Update as PbUpdate, WeightedAddr,
    },
    net::TcpAddress,
};

use control::{
    destination::{Metadata, ProtocolHint, Update},
    remote_stream::{self, Remote},
};

use identity;
use logging;
use never::Never;
use proxy::resolve;
use NameAddr;

use super::Client;

/// Holds the state of a single resolution.
pub struct Resolution {
    rx: mpsc::UnboundedReceiver<Update<Metadata>>,
    // auth: NameAddr,
    // cache: Cache,
    // inner: Option<Inner<T>>,
}

struct Daemon<T>
where
    T: GrpcService<BoxBody>,
{
    auth: NameAddr,
    client: Client<T>,
    query: Query<T>,
    tx: mpsc::UnboundedSender<Update<Metadata>>,
    addrs: IndexSet<SocketAddr>,
    /// Set to true on reconnects to indicate that the cache should be reset
    /// when next modified.
    should_reset: bool,
}

#[derive(Clone, Debug)]
struct LogCtx(NameAddr);

type Query<T> = remote_stream::Remote<PbUpdate, T>;

struct DisplayUpdate<'a>(&'a Update<Metadata>);

impl resolve::Resolution for Resolution {
    type Endpoint = Metadata;
    type Error = Never;

    fn poll(&mut self) -> Poll<Update<Self::Endpoint>, Self::Error> {
        trace!("poll resolution");
        match self.rx.poll() {
            Ok(Async::Ready(Some(up))) => Ok(Async::Ready(up)),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(_) | Ok(Async::Ready(None)) => {
                trace!("resolution daemon dropped; no endpoints exist");
                Ok(Async::NotReady)
            }
        }
    }
}

impl Resolution {
    pub(super) fn new<T>(auth: NameAddr, client: Client<T>) -> Self
    where
        T: GrpcService<BoxBody> + Send + 'static,
        T::ResponseBody: Send,
        <T::ResponseBody as Body>::Data: Send,
        T::Future: Send,
    {
        let (tx, rx) = mpsc::unbounded();
        let daemon = Daemon::new(auth.clone(), client, tx);
        let daemon = logging::admin().bg(LogCtx(auth)).future(daemon);
        tokio::spawn(Box::new(daemon));
        Self { rx }
    }

    pub(super) fn none() -> Self {
        let (_, rx) = mpsc::unbounded();
        Self { rx }
    }
}

// ===== impl Daemon =====
impl<T> Daemon<T>
where
    T: GrpcService<BoxBody> + Send,
{
    fn new(
        auth: NameAddr,
        mut client: Client<T>,
        tx: mpsc::UnboundedSender<Update<Metadata>>,
    ) -> Self {
        let query = client.query(&auth, "connect");
        Self {
            query,
            auth,
            client,
            tx,
            addrs: IndexSet::new(),
            should_reset: false,
        }
    }
}

macro_rules! try_send {
    ($this:expr, $up:expr) => {
        let up = $up;
        trace!("{} for {}", DisplayUpdate(&up), $this.auth);
        if let Err(_) = $this.tx.unbounded_send(up) {
            trace!("resolver for {} dropped, daemon terminating...", $this.auth);
            return Ok(Async::Ready(()));
        }
    };
}

impl<T> Future for Daemon<T>
where
    T: GrpcService<BoxBody>,
{
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            self.query = match self.query {
                Remote::ConnectedOrConnecting { ref mut rx } => match rx.poll() {
                    Ok(Async::Ready(Some(update))) => {
                        if self.should_reset {
                            self.should_reset = false;
                            for addr in self.addrs.drain(..) {
                                try_send!(self, Update::Remove(addr));
                            }
                        }

                        match update.update {
                            Some(PbUpdate2::Add(a_set)) => {
                                let set_labels = a_set.metric_labels;
                                let addrs = a_set
                                    .addrs
                                    .into_iter()
                                    .filter_map(|pb| pb_to_addr_meta(pb, &set_labels));
                                for (addr, meta) in addrs {
                                    self.addrs.insert(addr);
                                    try_send!(self, Update::Add(addr, meta));
                                }
                            }
                            Some(PbUpdate2::Remove(r_set)) => {
                                let addrs = r_set.addrs.into_iter().filter_map(pb_to_sock_addr);
                                for addr in addrs {
                                    self.addrs.remove(&addr);
                                    try_send!(self, Update::Remove(addr));
                                }
                            }
                            Some(PbUpdate2::NoEndpoints(_)) => {
                                try_send!(self, Update::NoEndpoints);
                                for addr in self.addrs.drain(..) {
                                    try_send!(self, Update::Remove(addr));
                                }
                            }
                            None => (),
                        };
                        continue;
                    }
                    Ok(Async::Ready(None)) => {
                        trace!(
                            "Destination.Get stream ended for {:?}, must reconnect",
                            self.auth
                        );
                        self.should_reset = true;
                        Remote::NeedsReconnect
                    }
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Err(ref status) if status.code() == tower_grpc::Code::InvalidArgument => {
                        // Invalid Argument is returned to indicate that the
                        // requested name should *not* query the destination
                        // service. In this case, do not attempt to reconnect.
                        debug!(
                            "Destination.Get stream ended for {:?} with Invalid Argument",
                            self.auth
                        );
                        let _ = self.tx.unbounded_send(Update::NoEndpoints);
                        return Ok(Async::Ready(()));
                    }
                    Err(err) => {
                        warn!(
                            "Destination.Get stream errored for {:?}: {:?}",
                            self.auth, err
                        );
                        self.should_reset = true;
                        Remote::NeedsReconnect
                    }
                },
                Remote::NeedsReconnect => {
                    if let Ok(Async::Ready(())) = self.client.client.poll_ready() {
                        self.client.query(&self.auth, "reconnect")
                    } else {
                        trace!("Destination client not yet ready to reconnect");
                        return Ok(Async::NotReady);
                    }
                }
            };
        }
    }
}

// // ===== impl Cache =====

// impl Cache {
//     fn next_update(&mut self) -> Option<Update<Metadata>> {
//         self.queue.pop_front()
//     }

//     fn add(&mut self, addrs: impl Iterator<Item = (SocketAddr, Metadata)>) {
//         self.maybe_reset();
//         for (addr, meta) in addrs {
//             self.queue.push_back(Update::Add(addr, meta));
//             self.addrs.insert(addr);
//         }
//     }

//     fn remove(&mut self, addrs: impl Iterator<Item = SocketAddr>) {
//         self.maybe_reset();
//         for addr in addrs {
//             self.queue.push_back(Update::Remove(addr));
//             self.addrs.remove(&addr);
//         }
//     }

//     fn should_reset(&mut self) {
//         trace!("cache should reset");
//         self.should_reset = true;
//     }

//     fn maybe_reset(&mut self) {
//         if self.should_reset {
//             self.should_reset = false;
//             self.queue.clear();
//             trace!("resetting {:?}", self.addrs);
//             for addr in self.addrs.drain(..) {
//                 self.queue.push_back(Update::Remove(addr));
//             }
//         }
//     }

//     fn no_endpoints(&mut self) {
//         self.queue.clear();
//         self.queue.push_front(Update::NoEndpoints);
//         for addr in self.addrs.drain(..) {
//             self.queue.push_back(Update::Remove(addr));
//         }
//     }
// }

// ===== impl Client =====

impl<T> Client<T>
where
    T: GrpcService<BoxBody>,
{
    /// Attepts to initiate a query to the Destination service if the given
    /// authority matches the client's set of search suffixes.
    ///
    /// # Returns
    /// - `None` if the authority is not suitable for querying the Destination
    //     service, or the underlying client service is `None`,
    /// - `Some(Query)` if the authority is suitable for querying the
    ///    Destination service.
    fn query(&mut self, dst: &NameAddr, connect_or_reconnect: &str) -> Query<T> {
        trace!("DestinationServiceQuery {} {:?}", connect_or_reconnect, dst);
        let req = GetDestination {
            scheme: "k8s".into(),
            path: format!("{}", dst),
            context_token: self.context_token.as_ref().clone(),
        };
        let mut svc = Destination::new(self.client.as_service());
        let response = svc.get(grpc::Request::new(req));
        let rx = remote_stream::Receiver::new(response);
        remote_stream::Remote::ConnectedOrConnecting { rx }
    }
}

impl<'a> fmt::Display for DisplayUpdate<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            Update::Remove(ref addr) => write!(f, "remove {}", addr),
            Update::Add(ref addr, ..) => write!(f, "add {}", addr),
            Update::NoEndpoints => "no endpoints".fmt(f),
        }
    }
}

impl fmt::Display for LogCtx {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "resolver addr={}", self.0)
    }
}

/// Construct a new labeled `SocketAddr `from a protobuf `WeightedAddr`.
fn pb_to_addr_meta(
    pb: WeightedAddr,
    set_labels: &HashMap<String, String>,
) -> Option<(SocketAddr, Metadata)> {
    let addr = pb.addr.and_then(pb_to_sock_addr)?;

    let meta = {
        let mut t = set_labels
            .iter()
            .chain(pb.metric_labels.iter())
            .collect::<Vec<(&String, &String)>>();
        t.sort_by(|(k0, _), (k1, _)| k0.cmp(k1));

        let mut m = IndexMap::with_capacity(t.len());
        for (k, v) in t.into_iter() {
            m.insert(k.clone(), v.clone());
        }

        m
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

    let tls_id = pb.tls_identity.and_then(pb_to_id);
    let meta = Metadata::new(meta, proto_hint, tls_id, pb.weight);
    Some((addr, meta))
}

fn pb_to_id(pb: TlsIdentity) -> Option<identity::Name> {
    use api::destination::tls_identity::Strategy;

    let Strategy::DnsLikeIdentity(i) = pb.strategy?;
    match identity::Name::from_hostname(i.name.as_bytes()) {
        Ok(i) => Some(i),
        Err(_) => {
            warn!("Ignoring invalid identity: {}", i.name);
            None
        }
    }
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
            }
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
            }
            None => None,
        },
        None => None,
    }
}

use super::client;
use crate::addr::NameAddr;
use crate::api::{
    destination::{
        protocol_hint::Protocol, update::Update as PbUpdate2, TlsIdentity, Update as PbUpdate,
        WeightedAddr,
    },
    net::TcpAddress,
};
use crate::core::resolve::{self, Update};
use crate::destination::{Metadata, ProtocolHint};
use crate::{identity, task, Never};
use futures::{
    future::Future,
    sync::{mpsc, oneshot},
    Async, Poll, Stream,
};
use indexmap::{IndexMap, IndexSet};
use std::{collections::HashMap, error::Error, fmt, net::SocketAddr};
use tower_grpc::{self as grpc, generic::client::GrpcService, Body, BoxBody};
use tracing::{debug, trace, warn};

/// A resolution for a single authority.
pub struct Resolution {
    rx: mpsc::UnboundedReceiver<Update<Metadata>>,
    _hangup: oneshot::Sender<Never>,
}

pub struct ResolveFuture<T>
where
    T: GrpcService<BoxBody>,
{
    query: Option<client::Query<T>>,
}

/// An error indicating that the Destination service cannot resolve the
/// requested name.
#[derive(Debug)]
pub struct Unresolvable {
    _p: (),
}

/// Drives the query associated with a `Resolution`.
///
/// Each destination service query is driven by its own background `Daemon`,
/// rather than in `Resolution::poll`, so that changes in the discovered
/// endpoints are handled as they are received, rather than only when polling
/// the resolution.
struct Daemon<T>
where
    T: GrpcService<BoxBody>,
{
    query: client::Query<T>,
    updater: Updater,
}

/// Updates the `Resolution` when the set of discovered endpoints changes.
///
/// This is more than just the send end of the channel, as it also tracks the
/// state necessary to reset stale endpoints when reconnecting.
struct Updater {
    tx: mpsc::UnboundedSender<Update<Metadata>>,
    /// This receiver is used to detect if the resolution has been dropped.
    hangup: oneshot::Receiver<Never>,
    /// All the endpoint addresses seen since the last reset.
    seen: IndexSet<SocketAddr>,
    /// Set to true on reconnects to indicate that previously seen addresses
    /// should be reset when the query reconnects.
    reset: bool,
}

#[derive(Clone, Debug)]
struct LogCtx(NameAddr);

struct DisplayUpdate<'a>(&'a Update<Metadata>);

impl resolve::Resolution for Resolution {
    type Endpoint = Metadata;
    type Error = Never;

    fn poll(&mut self) -> Poll<Update<Self::Endpoint>, Self::Error> {
        match self.rx.poll() {
            Ok(Async::Ready(Some(up))) => Ok(Async::Ready(up)),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(_) | Ok(Async::Ready(None)) => {
                trace!("resolution daemon has terminated");
                Ok(Async::NotReady)
            }
        }
    }
}

impl Resolution {
    fn new() -> (Self, Updater) {
        let (tx, rx) = mpsc::unbounded();

        // This oneshot allows the daemon to be notified when the Self::Stream
        // is dropped.
        let (hangup_tx, hangup_rx) = oneshot::channel();
        let resolution = Self {
            rx,
            _hangup: hangup_tx,
        };
        (resolution, Updater::new(tx, hangup_rx))
    }
}

// ===== impl ResolveFuture =====

impl<T> ResolveFuture<T>
where
    T: GrpcService<BoxBody> + Send,
{
    pub(super) fn new(authority: &NameAddr, client: client::Client<T>) -> Self {
        Self {
            query: Some(client.connect(&authority)),
        }
    }

    pub(super) fn unresolvable() -> Self {
        Self { query: None }
    }
}

impl<T> Future for ResolveFuture<T>
where
    T: GrpcService<BoxBody> + Send + 'static,
    T::ResponseBody: Send,
    <T::ResponseBody as Body>::Data: Send,
    T::Future: Send,
{
    type Item = Resolution;
    type Error = Unresolvable;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let update = match self.query {
                Some(ref mut query) => match query.poll() {
                    Ok(Async::Ready(Some(up))) => up,
                    Ok(Async::Ready(None)) => {
                        warn!("Destination.Get stream ended immediately, must reconnect");
                        query.reconnect();
                        continue;
                    }
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Err(ref status) if status.code() == grpc::Code::InvalidArgument => {
                        trace!("{} is unresolvable", query.authority());
                        return Err(Unresolvable { _p: () });
                    }
                    Err(err) => {
                        warn!("Destination.Get stream error {}, must reconnect", err);
                        query.reconnect();
                        continue;
                    }
                },
                None => {
                    trace!("name is unresolvable");
                    return Err(Unresolvable { _p: () });
                }
            };

            let (res, mut updater) = Resolution::new();
            updater
                .update(update)
                .expect("resolution should not have been dropped");

            let query = self.query.take().expect("invalid state");

            // todo: set logging context within the daemon task.
            task::spawn(Daemon { updater, query });

            return Ok(Async::Ready(res));
        }
    }
}

// ===== impl Daemon =====

impl<T> Future for Daemon<T>
where
    T: GrpcService<BoxBody>,
{
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.updater.hangup.poll() {
            Ok(Async::Ready(never)) => match never {}, // unreachable!
            Ok(Async::NotReady) => {}
            Err(_) => {
                // Hangup tx has been dropped.
                debug!("resolution cancelled");
                return Ok(Async::Ready(()));
            }
        };

        loop {
            match self.query.poll() {
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Ok(Async::Ready(Some(update))) => {
                    if let Err(_) = self.updater.update(update) {
                        trace!("resolution dropped, daemon terminating...");
                        return Ok(Async::Ready(()));
                    }
                }
                Ok(Async::Ready(None)) => {
                    trace!("Destination.Get stream ended, must reconnect");
                    self.updater.should_reset();
                    self.query.reconnect();
                }
                Err(err) => {
                    warn!("Destination.Get stream error: {}", err);
                    self.updater.should_reset();
                    self.query.reconnect();
                }
            }
        }
    }
}

// ===== impl Updater =====

impl Updater {
    fn new(tx: mpsc::UnboundedSender<Update<Metadata>>, hangup: oneshot::Receiver<Never>) -> Self {
        Self {
            tx,
            hangup,
            seen: IndexSet::new(),
            reset: false,
        }
    }

    fn update(&mut self, update: PbUpdate) -> Result<(), ()> {
        match update.update {
            Some(PbUpdate2::Add(a_set)) => {
                let set_labels = a_set.metric_labels;
                let addrs = a_set
                    .addrs
                    .into_iter()
                    .filter_map(|pb| pb_to_addr_meta(pb, &set_labels));
                self.add(addrs)?;
            }
            Some(PbUpdate2::Remove(r_set)) => {
                let addrs = r_set.addrs.into_iter().filter_map(pb_to_sock_addr);
                self.remove(addrs)?;
            }
            Some(PbUpdate2::NoEndpoints(_)) => {
                trace!("has no endpoints");
                self.remove_all("no endpoints")?;
            }
            None => (),
        }
        Ok(())
    }

    fn send(&mut self, update: Update<Metadata>) -> Result<(), ()> {
        trace!("{}", DisplayUpdate(&update));
        self.tx.unbounded_send(update).map_err(|_| ())
    }

    /// Indicates that the resolution should reset any previously discovered
    /// endpoints on the next update received after a reconnect.
    fn should_reset(&mut self) {
        self.reset = true;
    }

    /// Removes any previously discovered endpoints if they are stale.
    /// Otherwise, does nothing.
    ///
    /// This is called when processing a new update.
    fn reset_if_needed(&mut self) -> Result<(), ()> {
        if self.reset {
            trace!("query reconnected; removing stale endpoints");
            self.remove_all("stale")?;
            self.reset = false;
        }
        Ok(())
    }

    fn add(&mut self, addrs: impl Iterator<Item = (SocketAddr, Metadata)>) -> Result<(), ()> {
        self.reset_if_needed()?;
        for (addr, meta) in addrs {
            self.seen.insert(addr);
            self.send(Update::Add(addr, meta))?;
        }
        Ok(())
    }

    fn remove(&mut self, addrs: impl Iterator<Item = SocketAddr>) -> Result<(), ()> {
        self.reset_if_needed()?;
        for addr in addrs {
            self.seen.remove(&addr);
            self.send(Update::Remove(addr))?;
        }
        Ok(())
    }

    fn remove_all(&mut self, reason: &'static str) -> Result<(), ()> {
        for addr in self.seen.drain(..) {
            trace!("remove {} ({})", addr, reason);
            self.tx
                .unbounded_send(Update::Remove(addr))
                .map_err(|_| ())?;
        }
        Ok(())
    }
}

impl<'a> fmt::Display for DisplayUpdate<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Update::Remove(ref addr) => write!(f, "remove {}", addr),
            Update::Add(ref addr, ..) => write!(f, "add {}", addr),
        }
    }
}

impl fmt::Display for LogCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "resolver addr={}", self.0)
    }
}

// === impl Unresolvable ===

impl Unresolvable {
    pub fn new() -> Self {
        Self { _p: () }
    }
}

impl fmt::Display for Unresolvable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "this name cannot be resolved by the destination service".fmt(f)
    }
}

impl Error for Unresolvable {}

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
    use crate::api::destination::tls_identity::Strategy;

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
    use crate::api::net::ip_address::Ip;
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

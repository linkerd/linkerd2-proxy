use futures::{future::Future, sync::mpsc, Async, Poll, Stream};
use indexmap::{IndexMap, IndexSet};
use std::{collections::HashMap, fmt, net::SocketAddr};

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

/// A resolution for a single authority.
pub struct Resolution {
    rx: mpsc::UnboundedReceiver<Update<Metadata>>,
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
    auth: NameAddr,
    client: Client<T>,
    query: Query<T>,
    updater: Updater,
}

/// Updates the `Resolution` when the set of discovered endpoints changes.
///
/// This is more than just the send end of the channel, as it also tracks the
/// state necessary to reset stale endpoints when reconnecting.
struct Updater {
    tx: mpsc::UnboundedSender<Update<Metadata>>,
    /// All the endpoint addresses seen since the last reset.
    seen: IndexSet<SocketAddr>,
    /// Set to true on reconnects to indicate that previously seen addresses
    /// should be reset when the query reconnects.
    reset: bool,
}

#[derive(Clone, Debug)]
struct LogCtx(NameAddr);

struct DisplayUpdate<'a>(&'a Update<Metadata>);

type Query<T> = remote_stream::Remote<PbUpdate, T>;

impl resolve::Resolution for Resolution {
    type Endpoint = Metadata;
    type Error = Never;

    fn poll(&mut self) -> Poll<Update<Self::Endpoint>, Self::Error> {
        match self.rx.poll() {
            Ok(Async::Ready(Some(up))) => Ok(Async::Ready(up)),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(_) | Ok(Async::Ready(None)) => {
                trace!("resolution daemon has terminated; no endpoints exist");
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
        let daemon = logging::Section::Proxy.bg(LogCtx(auth)).future(daemon);
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
            updater: Updater::new(tx),
        }
    }
}

impl<T> Future for Daemon<T>
where
    T: GrpcService<BoxBody>,
{
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // Try to send an update to the `Resolution`, ending the background task
        // if the resolution is no longer needed.
        macro_rules! try_send {
            ($send:expr) => {
                if let Err(_) = $send {
                    trace!("resolution dropped, daemon terminating...");
                    return Ok(Async::Ready(()));
                }
            };
        }
        loop {
            self.query = match self.query {
                Remote::ConnectedOrConnecting { ref mut rx } => match rx.poll() {
                    Ok(Async::Ready(Some(update))) => {
                        match update.update {
                            Some(PbUpdate2::Add(a_set)) => {
                                let set_labels = a_set.metric_labels;
                                let addrs = a_set
                                    .addrs
                                    .into_iter()
                                    .filter_map(|pb| pb_to_addr_meta(pb, &set_labels));
                                try_send!(self.updater.add(addrs));
                            }
                            Some(PbUpdate2::Remove(r_set)) => {
                                let addrs = r_set.addrs.into_iter().filter_map(pb_to_sock_addr);
                                try_send!(self.updater.remove(addrs));
                            }
                            Some(PbUpdate2::NoEndpoints(_)) => {
                                try_send!(self.updater.no_endpoints())
                            }
                            None => (),
                        };
                        continue;
                    }
                    Ok(Async::Ready(None)) => {
                        trace!("Destination.Get stream ended, must reconnect");
                        self.updater.should_reset();
                        Remote::NeedsReconnect
                    }
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Err(ref status) if status.code() == tower_grpc::Code::InvalidArgument => {
                        // Invalid Argument is returned to indicate that the
                        // requested name should *not* query the destination
                        // service. In this case, do not attempt to reconnect.
                        debug!("Destination.Get stream ended with Invalid Argument",);
                        let _ = self.updater.no_endpoints();
                        return Ok(Async::Ready(()));
                    }
                    Err(err) => {
                        warn!("Destination.Get stream error: {}", err);
                        self.updater.should_reset();
                        Remote::NeedsReconnect
                    }
                },
                Remote::NeedsReconnect => match self.client.query(&self.auth, "reconnect") {
                    Remote::NeedsReconnect => return Ok(Async::NotReady),
                    query => query,
                },
            };
        }
    }
}

// ===== impl Client =====

impl<T> Client<T>
where
    T: GrpcService<BoxBody>,
{
    /// Returns a new destination service query for the given `dst`.
    fn query(&mut self, dst: &NameAddr, connect_or_reconnect: &str) -> Query<T> {
        trace!(
            "{}ing destination service query for {}",
            connect_or_reconnect,
            dst
        );
        if let Ok(Async::Ready(())) = self.client.poll_ready() {
            let req = GetDestination {
                scheme: "k8s".into(),
                path: format!("{}", dst),
                context_token: self.context_token.as_ref().clone(),
            };
            let mut svc = Destination::new(self.client.as_service());
            let response = svc.get(grpc::Request::new(req));
            let rx = remote_stream::Receiver::new(response);
            Remote::ConnectedOrConnecting { rx }
        } else {
            trace!("destination client not yet ready");
            Remote::NeedsReconnect
        }
    }
}

// ===== impl Updater =====

impl Updater {
    fn new(tx: mpsc::UnboundedSender<Update<Metadata>>) -> Self {
        Self {
            tx,
            seen: IndexSet::new(),
            reset: false,
        }
    }

    fn send(&mut self, update: Update<Metadata>) -> Result<(), ()> {
        trace!("{}", DisplayUpdate(&update));
        self.tx.unbounded_send(update).map_err(|_| ())
    }

    /// Indicates that the resolution should be reset on the next update
    /// received after a reconnect.
    fn should_reset(&mut self) {
        self.reset = true;
    }

    /// If the
    fn reset_if_needed(&mut self) -> Result<(), ()> {
        if self.reset {
            trace!("query reconnected; removing stale endpoints");
            for addr in self.seen.drain(..) {
                trace!("remove {} (stale)", addr);
                self.tx
                    .unbounded_send(Update::Remove(addr))
                    .map_err(|_| ())?;
            }
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

    fn no_endpoints(&mut self) -> Result<(), ()> {
        self.send(Update::NoEndpoints)?;
        for addr in self.seen.drain(..) {
            trace!("remove {} (no endpoints)", addr);
            self.tx
                .unbounded_send(Update::Remove(addr))
                .map_err(|_| ())?;
        }
        Ok(())
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

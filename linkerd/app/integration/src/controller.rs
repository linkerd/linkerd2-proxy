use super::*;

use linkerd2_proxy_api::destination as pb;
use linkerd2_proxy_api::net;
use linkerd_app_core::proxy::http::trace;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::ops::{Bound, RangeBounds};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic as grpc;
use tracing::instrument::Instrument;

pub fn new() -> Controller {
    Controller::new()
}

pub fn new_unordered() -> Controller {
    Controller::new_unordered()
}

pub fn identity() -> identity::Controller {
    identity::Controller::default()
}

pub type Labels = HashMap<String, String>;

pub type DstReceiver = UnboundedReceiverStream<Result<pb::Update, grpc::Status>>;

#[derive(Clone, Debug)]
pub struct DstSender(mpsc::UnboundedSender<Result<pb::Update, grpc::Status>>);

pub type ProfileReceiver = UnboundedReceiverStream<Result<pb::DestinationProfile, grpc::Status>>;

#[derive(Clone, Debug)]
pub struct ProfileSender(mpsc::UnboundedSender<Result<pb::DestinationProfile, grpc::Status>>);

#[derive(Clone, Debug, Default)]
pub struct Controller {
    expect_dst_calls: Arc<Mutex<VecDeque<Dst>>>,
    expect_profile_calls: Arc<Mutex<VecDeque<(pb::GetDestination, ProfileReceiver)>>>,
    unordered: bool,
}

pub struct Listening {
    pub addr: SocketAddr,
    drain: drain::Signal,
    task: tokio::task::JoinHandle<Result<(), std::io::Error>>,
    name: &'static str,
}

#[derive(Clone, Debug, Default)]
pub struct RouteBuilder {
    route: pb::Route,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
enum Dst {
    Call(pb::GetDestination, Result<DstReceiver, grpc::Status>),
    Done,
}

impl Controller {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_unordered() -> Self {
        Self {
            unordered: true,
            ..Self::default()
        }
    }

    pub fn destination_tx(&self, dest: impl Into<String>) -> DstSender {
        let (tx, rx) = mpsc::unbounded_channel();
        let rx = UnboundedReceiverStream::new(rx);
        let mut path = dest.into();
        if !path.contains(':') {
            path.push_str(":80");
        };
        let dst = pb::GetDestination {
            path,
            ..Default::default()
        };
        self.expect_dst_calls
            .lock()
            .push_back(Dst::Call(dst, Ok(rx)));
        DstSender(tx)
    }

    pub fn destination_tx_err(&self, dest: impl Into<String>, err: grpc::Code) -> DstSender {
        let tx = self.destination_tx(dest);
        tx.send_err(grpc::Status::new(err, "unit test controller fake error"));
        tx
    }

    pub fn destination_fail(&self, dest: impl Into<String>, status: grpc::Status) {
        let mut path = dest.into();
        if !path.contains(':') {
            path.push_str(":80");
        };
        let dst = pb::GetDestination {
            path,
            ..Default::default()
        };
        self.expect_dst_calls
            .lock()
            .push_back(Dst::Call(dst, Err(status)));
    }

    pub fn no_more_destinations(&self) {
        self.expect_dst_calls.lock().push_back(Dst::Done);
    }

    pub async fn delay_listen<F>(self, f: F) -> Listening
    where
        F: Future<Output = ()> + Send + 'static,
    {
        run(
            pb::destination_server::DestinationServer::new(self),
            "support destination service",
            Some(Box::pin(f)),
        )
        .await
    }

    pub fn profile_tx_default(&self, target: impl ToString, dest: &str) -> ProfileSender {
        let tx = self.profile_tx(&target.to_string());
        tx.send(pb::DestinationProfile {
            fully_qualified_name: dest.to_owned(),
            ..Default::default()
        });
        tx
    }

    pub fn profile_tx(&self, dest: impl Into<String>) -> ProfileSender {
        let (tx, rx) = mpsc::unbounded_channel();
        let rx = UnboundedReceiverStream::new(rx);
        let mut path = dest.into();
        if !path.contains(':') {
            path.push_str(":80");
        };
        let dst = pb::GetDestination {
            path,
            ..Default::default()
        };
        self.expect_profile_calls.lock().push_back((dst, rx));
        ProfileSender(tx)
    }

    pub async fn run(self) -> Listening {
        run(
            pb::destination_server::DestinationServer::new(self),
            "support destination service",
            None,
        )
        .await
    }
}

fn grpc_no_results() -> grpc::Status {
    grpc::Status::new(
        grpc::Code::Unavailable,
        "unit test controller has no results",
    )
}

fn grpc_unexpected_request() -> grpc::Status {
    grpc::Status::new(
        grpc::Code::Unavailable,
        "unit test controller expected different request",
    )
}

impl DstSender {
    pub fn send(&self, up: pb::Update) {
        self.0.send(Ok(up)).expect("send dst update")
    }

    pub fn send_err(&self, e: grpc::Status) {
        self.0.send(Err(e)).expect("send dst err")
    }

    pub fn send_addr(&self, addr: SocketAddr) {
        self.send(destination_add(addr))
    }

    pub fn send_labeled(&self, addr: SocketAddr, addr_labels: Labels, parent_labels: Labels) {
        self.send(destination_add_labeled(
            addr,
            Hint::Unknown,
            addr_labels,
            parent_labels,
        ));
    }

    pub fn send_h2_hinted(&self, addr: SocketAddr) {
        self.send(destination_add_hinted(addr, Hint::H2));
    }

    pub fn send_no_endpoints(&self) {
        self.send(destination_exists_with_no_endpoints())
    }
}

impl ProfileSender {
    pub fn send(&self, up: pb::DestinationProfile) {
        self.0.send(Ok(up)).expect("send profile update")
    }

    pub fn send_err(&self, err: grpc::Status) {
        self.0.send(Err(err)).expect("send profile update")
    }
}

#[tonic::async_trait]
impl pb::destination_server::Destination for Controller {
    type GetStream = DstReceiver;

    async fn get(
        &self,
        req: grpc::Request<pb::GetDestination>,
    ) -> Result<grpc::Response<Self::GetStream>, grpc::Status> {
        let span = tracing::info_span!("Destination::get", req.path = &req.get_ref().path[..]);
        let _e = span.enter();
        tracing::debug!(request = ?req.get_ref(), "received");

        let mut calls = self.expect_dst_calls.lock();
        if self.unordered {
            let mut calls_next: VecDeque<Dst> = VecDeque::new();
            if calls.is_empty() {
                tracing::warn!("calls exhausted");
            }
            while let Some(call) = calls.pop_front() {
                if let Dst::Call(dst, updates) = call {
                    tracing::debug!(?dst, "checking");
                    if &dst == req.get_ref() {
                        tracing::info!(?dst, ?updates, "found request");
                        calls_next.extend(calls.drain(..));
                        *calls = calls_next;
                        return updates.map(grpc::Response::new);
                    }

                    calls_next.push_back(Dst::Call(dst, updates));
                }
            }

            tracing::warn!(remaining = calls_next.len(), "missed");
            *calls = calls_next;
            return Err(grpc_unexpected_request());
        }

        match calls.pop_front() {
            Some(Dst::Call(dst, updates)) => {
                tracing::debug!(?dst, "checking next call");
                if &dst == req.get_ref() {
                    tracing::info!(?dst, ?updates, "found request");
                    return updates.map(grpc::Response::new);
                }

                tracing::warn!(?dst, ?updates, "request does not match");
                let msg = format!(
                    "expected get call for {:?} but got get call for {:?}",
                    dst, req
                );
                calls.push_front(Dst::Call(dst, updates));
                return Err(grpc::Status::new(grpc::Code::Unavailable, msg));
            }
            Some(Dst::Done) => {
                panic!("unit test controller expects no more Destination.Get calls")
            }
            _ => {}
        }

        Err(grpc_no_results())
    }

    type GetProfileStream =
        Pin<Box<dyn Stream<Item = Result<pb::DestinationProfile, grpc::Status>> + Send + Sync>>;

    async fn get_profile(
        &self,
        req: grpc::Request<pb::GetDestination>,
    ) -> Result<grpc::Response<Self::GetProfileStream>, grpc::Status> {
        let span = tracing::info_span!(
            "Destination::get_profile",
            req.path = &req.get_ref().path[..]
        );
        let _e = span.enter();
        tracing::debug!(request = ?req.get_ref(), "received");
        let mut calls = self.expect_profile_calls.lock();
        if let Some((dst, profile)) = calls.pop_front() {
            tracing::debug!(?dst, "checking next call");
            if &dst == req.get_ref() {
                tracing::info!(?dst, ?profile, "found request");
                return Ok(grpc::Response::new(Box::pin(profile)));
            }

            tracing::warn!(?dst, ?profile, "request does not match");
            calls.push_front((dst, profile));
            return Err(grpc_unexpected_request());
        }

        Err(grpc_no_results())
    }
}

impl Listening {
    pub async fn join(self) {
        let span = tracing::info_span!("join", controller = %self.name, addr = %self.addr);
        async move {
            tracing::debug!("shutting down...");
            self.drain.drain().await;
            tracing::debug!("drained!");

            tracing::debug!("waiting for task to complete...");
            match self.task.await {
                Ok(res) => res.expect("support controller failed"),
                // If the task panicked, propagate the panic so that the test can
                // fail nicely.
                Err(err) if err.is_panic() => {
                    tracing::error!("support {} panicked!", self.name);
                    std::panic::resume_unwind(err.into_panic());
                }
                // If the task was already canceled, it was probably shut down
                // explicitly, that's fine.
                Err(_) => tracing::debug!("support server task already canceled"),
            }

            tracing::info!("support {} on {} terminated cleanly", self.name, self.addr);
        }
        .instrument(span)
        .await
    }
}

pub(in crate) async fn run<T, B>(
    svc: T,
    name: &'static str,
    delay: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
) -> Listening
where
    T: tower::Service<http::Request<hyper::body::Body>, Response = http::Response<B>>,
    T: Clone + Send + Sync + 'static,
    T::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    T::Future: Send,
    B: http_body::Body + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    B::Data: Send + 'static,
{
    let (listening_tx, listening_rx) = tokio::sync::oneshot::channel();
    let (drain_signal, drain) = drain::channel();

    // Bind an ephemeral port but do not start listening yet.
    let (sock, addr) = crate::bind_ephemeral();

    let task = tokio::spawn(
        cancelable(drain.clone(), async move {
            // Start listening on the socket.
            let listener = crate::listen(sock);
            let mut listening_tx = Some(listening_tx);

            if let Some(delay) = delay {
                let _ = listening_tx.take().unwrap().send(());
                delay.await;
            }

            if let Some(listening_tx) = listening_tx {
                let _ = listening_tx.send(());
            }

            let mut http = hyper::server::conn::Http::new().with_executor(trace::Executor::new());
            http.http2_only(true);
            loop {
                let (sock, addr) = listener.accept().await?;
                let span = tracing::debug_span!("conn", %addr);
                let serve = http.serve_connection(sock, svc.clone());
                let f = async move {
                    serve
                        .await
                        .map_err(|error| tracing::error!(%error, "serving connection failed."))?;
                    Ok::<(), ()>(())
                };
                tokio::spawn(cancelable(drain.clone(), f).instrument(span.or_current()));
            }
        })
        .instrument(tracing::info_span!("controller", message = %name, %addr).or_current()),
    );

    listening_rx.await.expect("listening_rx");
    tracing::info!(%addr, "{} listening", name);

    Listening {
        addr,
        drain: drain_signal,
        task,
        name,
    }
}

pub enum Hint {
    Unknown,
    H2,
}

pub fn destination_add(addr: SocketAddr) -> pb::Update {
    destination_add_hinted(addr, Hint::Unknown)
}

pub fn destination_add_hinted(addr: SocketAddr, hint: Hint) -> pb::Update {
    destination_add_labeled(addr, hint, HashMap::new(), HashMap::new())
}

pub fn destination_add_labeled(
    addr: SocketAddr,
    hint: Hint,
    set_labels: HashMap<String, String>,
    addr_labels: HashMap<String, String>,
) -> pb::Update {
    let protocol_hint = match hint {
        Hint::Unknown => None,
        Hint::H2 => Some(pb::ProtocolHint {
            protocol: Some(pb::protocol_hint::Protocol::H2(pb::protocol_hint::H2 {})),
            ..Default::default()
        }),
    };
    pb::Update {
        update: Some(pb::update::Update::Add(pb::WeightedAddrSet {
            addrs: vec![pb::WeightedAddr {
                addr: Some(net::TcpAddress {
                    ip: Some(ip_conv(addr.ip())),
                    port: u32::from(addr.port()),
                }),
                weight: 0,
                metric_labels: addr_labels,
                protocol_hint,
                ..Default::default()
            }],
            metric_labels: set_labels,
        })),
    }
}

pub fn destination_add_tls(addr: SocketAddr, local_id: &str) -> pb::Update {
    pb::Update {
        update: Some(pb::update::Update::Add(pb::WeightedAddrSet {
            addrs: vec![pb::WeightedAddr {
                addr: Some(net::TcpAddress {
                    ip: Some(ip_conv(addr.ip())),
                    port: u32::from(addr.port()),
                }),
                tls_identity: Some(pb::TlsIdentity {
                    strategy: Some(pb::tls_identity::Strategy::DnsLikeIdentity(
                        pb::tls_identity::DnsLikeIdentity {
                            name: local_id.into(),
                        },
                    )),
                }),
                ..Default::default()
            }],
            ..Default::default()
        })),
    }
}

pub fn destination_add_none() -> pb::Update {
    pb::Update {
        update: Some(pb::update::Update::Add(pb::WeightedAddrSet {
            addrs: Vec::new(),
            ..Default::default()
        })),
    }
}

pub fn destination_remove_none() -> pb::Update {
    pb::Update {
        update: Some(pb::update::Update::Remove(pb::AddrSet {
            addrs: Vec::new(),
        })),
    }
}

pub fn destination_exists_with_no_endpoints() -> pb::Update {
    pb::Update {
        update: Some(pb::update::Update::NoEndpoints(pb::NoEndpoints {
            exists: true,
        })),
    }
}

pub fn destination_does_not_exist() -> pb::Update {
    pb::Update {
        update: Some(pb::update::Update::NoEndpoints(pb::NoEndpoints {
            exists: false,
        })),
    }
}

pub fn profile<I>(
    routes: I,
    retry_budget: Option<pb::RetryBudget>,
    dst_overrides: Vec<pb::WeightedDst>,
    fqn: impl Into<String>,
) -> pb::DestinationProfile
where
    I: IntoIterator,
    I::Item: Into<pb::Route>,
{
    let routes = routes.into_iter().map(Into::into).collect();
    pb::DestinationProfile {
        routes,
        retry_budget,
        dst_overrides,
        fully_qualified_name: fqn.into(),
        ..Default::default()
    }
}

pub fn retry_budget(
    ttl: Duration,
    retry_ratio: f32,
    min_retries_per_second: u32,
) -> pb::RetryBudget {
    pb::RetryBudget {
        ttl: Some(ttl.into()),
        retry_ratio,
        min_retries_per_second,
    }
}

pub fn dst_override(authority: String, weight: u32) -> pb::WeightedDst {
    pb::WeightedDst { authority, weight }
}

pub fn route() -> RouteBuilder {
    RouteBuilder::default()
}

impl RouteBuilder {
    pub fn request_any(self) -> Self {
        self.request_path(".*")
    }

    pub fn request_path(mut self, path: &str) -> Self {
        let path_match = pb::PathMatch {
            regex: String::from(path),
        };
        self.route.condition = Some(pb::RequestMatch {
            r#match: Some(pb::request_match::Match::Path(path_match)),
        });
        self
    }

    pub fn label(mut self, key: &str, val: &str) -> Self {
        self.route.metrics_labels.insert(key.into(), val.into());
        self
    }

    fn response_class(mut self, condition: pb::ResponseMatch, is_failure: bool) -> Self {
        self.route.response_classes.push(pb::ResponseClass {
            condition: Some(condition),
            is_failure,
        });
        self
    }

    fn response_class_status(self, status_range: impl RangeBounds<u16>, is_failure: bool) -> Self {
        let min = match status_range.start_bound() {
            Bound::Included(&min) => min,
            Bound::Excluded(&min) => min + 1,
            Bound::Unbounded => 100,
        }
        .into();
        let max = match status_range.end_bound() {
            Bound::Included(&max) => max,
            Bound::Excluded(&max) => max - 1,
            Bound::Unbounded => 599,
        }
        .into();
        assert!(min >= 100 && min <= max);
        assert!(max <= 599);
        let range = pb::HttpStatusRange { min, max };
        let condition = pb::ResponseMatch {
            r#match: Some(pb::response_match::Match::Status(range)),
        };
        self.response_class(condition, is_failure)
    }

    pub fn response_success(self, status_range: impl RangeBounds<u16>) -> Self {
        self.response_class_status(status_range, false)
    }

    pub fn response_failure(self, status_range: impl RangeBounds<u16>) -> Self {
        self.response_class_status(status_range, true)
    }

    pub fn retryable(mut self, is: bool) -> Self {
        self.route.is_retryable = is;
        self
    }

    pub fn timeout(mut self, dur: Duration) -> Self {
        self.route.timeout = Some(dur.into());
        self
    }
}

impl From<RouteBuilder> for pb::Route {
    fn from(rb: RouteBuilder) -> Self {
        rb.route
    }
}

fn ip_conv(ip: IpAddr) -> net::IpAddress {
    match ip {
        IpAddr::V4(v4) => net::IpAddress {
            ip: Some(net::ip_address::Ip::Ipv4(v4.into())),
        },
        IpAddr::V6(v6) => {
            let (first, last) = octets_to_u64s(v6.octets());
            net::IpAddress {
                ip: Some(net::ip_address::Ip::Ipv6(net::IPv6 { first, last })),
            }
        }
    }
}

fn octets_to_u64s(octets: [u8; 16]) -> (u64, u64) {
    let first = (u64::from(octets[0]) << 56)
        + (u64::from(octets[1]) << 48)
        + (u64::from(octets[2]) << 40)
        + (u64::from(octets[3]) << 32)
        + (u64::from(octets[4]) << 24)
        + (u64::from(octets[5]) << 16)
        + (u64::from(octets[6]) << 8)
        + u64::from(octets[7]);
    let last = (u64::from(octets[8]) << 56)
        + (u64::from(octets[9]) << 48)
        + (u64::from(octets[10]) << 40)
        + (u64::from(octets[11]) << 32)
        + (u64::from(octets[12]) << 24)
        + (u64::from(octets[13]) << 16)
        + (u64::from(octets[14]) << 8)
        + u64::from(octets[15]);
    (first, last)
}

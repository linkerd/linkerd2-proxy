use super::*;

use linkerd2_app_core::proxy::http::trace;
use linkerd2_proxy_api_tonic::destination as pb;
use linkerd2_proxy_api_tonic::net;
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::ops::{Bound, RangeBounds};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tonic as grpc;
use tracing_futures::Instrument;

pub fn new() -> Controller {
    Controller::new()
}

pub fn new_unordered() -> Controller {
    Controller::new_unordered()
}

pub fn identity() -> identity::Controller {
    identity::Controller::new()
}

pub type Labels = HashMap<String, String>;

pub type DstReceiver = mpsc::UnboundedReceiver<Result<pb::Update, grpc::Status>>;

#[derive(Clone, Debug)]
pub struct DstSender(mpsc::UnboundedSender<Result<pb::Update, grpc::Status>>);

pub type ProfileReceiver = mpsc::UnboundedReceiver<pb::DestinationProfile>;

#[derive(Clone, Debug)]
pub struct ProfileSender(mpsc::UnboundedSender<pb::DestinationProfile>);

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
enum Dst {
    Call(pb::GetDestination, Result<DstReceiver, grpc::Status>),
    Done,
}

impl Controller {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_unordered() -> Self {
        let mut ctrl = Self::default();
        ctrl.unordered = true;
        ctrl
    }

    pub fn destination_tx(&self, dest: &str) -> DstSender {
        let (tx, rx) = mpsc::unbounded_channel();
        let path = if dest.contains(":") {
            dest.to_owned()
        } else {
            format!("{}:80", dest)
        };
        let dst = pb::GetDestination {
            path,
            ..Default::default()
        };
        self.expect_dst_calls
            .lock()
            .unwrap()
            .push_back(Dst::Call(dst, Ok(rx)));
        DstSender(tx)
    }

    pub fn destination_tx_err(&self, dest: &str, err: grpc::Code) -> DstSender {
        let tx = self.destination_tx(dest);
        tx.send_err(grpc::Status::new(err, "unit test controller fake error"));
        tx
    }

    pub fn destination_fail(&self, dest: &str, status: grpc::Status) {
        let path = if dest.contains(":") {
            dest.to_owned()
        } else {
            format!("{}:80", dest)
        };
        let dst = pb::GetDestination {
            path,
            ..Default::default()
        };
        self.expect_dst_calls
            .lock()
            .unwrap()
            .push_back(Dst::Call(dst, Err(status)));
    }

    pub fn no_more_destinations(&self) {
        self.expect_dst_calls.lock().unwrap().push_back(Dst::Done);
    }

    pub fn delay_listen<F>(self, f: F) -> Listening
    where
        F: Future<Output = ()> + Send + 'static,
    {
        run(
            pb::destination_server::DestinationServer::new(self),
            "support destination service",
            Some(Box::pin(f)),
        )
    }

    pub fn profile_tx_default(&self, dest: &str) -> ProfileSender {
        let tx = self.profile_tx(dest);
        tx.send(pb::DestinationProfile::default());
        tx
    }

    pub fn profile_tx(&self, dest: &str) -> ProfileSender {
        let (tx, rx) = mpsc::unbounded_channel();
        let path = if dest.contains(":") {
            dest.to_owned()
        } else {
            format!("{}:80", dest)
        };
        let dst = pb::GetDestination {
            path,
            ..Default::default()
        };
        self.expect_profile_calls
            .lock()
            .unwrap()
            .push_back((dst, rx));
        ProfileSender(tx)
    }

    pub fn run(self) -> Listening {
        run(
            pb::destination_server::DestinationServer::new(self),
            "support destination service",
            None,
        )
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
        self.0.send(up).expect("send profile update")
    }
}

#[tonic::async_trait]
impl pb::destination_server::Destination for Controller {
    type GetStream = DstReceiver;

    async fn get(
        &self,
        req: grpc::Request<pb::GetDestination>,
    ) -> Result<grpc::Response<Self::GetStream>, grpc::Status> {
        if let Ok(mut calls) = self.expect_dst_calls.lock() {
            if self.unordered {
                let mut calls_next: VecDeque<Dst> = VecDeque::new();
                if calls.is_empty() {
                    tracing::warn!("exhausted request={:?}", req.get_ref());
                }
                while let Some(call) = calls.pop_front() {
                    if let Dst::Call(dst, updates) = call {
                        if &dst == req.get_ref() {
                            tracing::info!("found request={:?}", dst);
                            calls_next.extend(calls.drain(..));
                            *calls = calls_next;
                            return updates.map(grpc::Response::new);
                        }

                        calls_next.push_back(Dst::Call(dst, updates));
                    }
                }

                tracing::warn!("missed request={:?} remaining={:?}", req, calls_next.len());
                *calls = calls_next;
                return Err(grpc_unexpected_request());
            }

            match calls.pop_front() {
                Some(Dst::Call(dst, updates)) => {
                    if &dst == req.get_ref() {
                        tracing::debug!("found request={:?}", dst);
                        return updates.map(grpc::Response::new);
                    }

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
        }

        Err(grpc_no_results())
    }

    type GetProfileStream =
        Pin<Box<dyn Stream<Item = Result<pb::DestinationProfile, grpc::Status>> + Send + Sync>>;

    async fn get_profile(
        &self,
        req: grpc::Request<pb::GetDestination>,
    ) -> Result<grpc::Response<Self::GetProfileStream>, grpc::Status> {
        if let Ok(mut calls) = self.expect_profile_calls.lock() {
            if let Some((dst, profile)) = calls.pop_front() {
                if &dst == req.get_ref() {
                    return Ok(grpc::Response::new(Box::pin(profile.map(Ok))));
                }

                calls.push_front((dst, profile));
                return Err(grpc_unexpected_request());
            }
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

pub(in crate) fn run<T, B>(
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
    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let listener = net2::TcpBuilder::new_v4().expect("Tcp::new_v4");
    listener.bind(addr).expect("Tcp::bind");
    let addr = listener.local_addr().expect("Tcp::local_addr");
    let listener = listener.listen(1024).expect("listen");
    let (drain_signal, drain) = drain::channel();
    let task = tokio::spawn(
        cancelable(drain.clone(), async move {
            let mut listener = tokio::net::TcpListener::from_std(listener)?;

            if let Some(delay) = delay {
                delay.await;
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
                tokio::spawn(cancelable(drain.clone(), f).instrument(span));
            }
        })
        .instrument(tracing::info_span!("controller", %name, %addr)),
    );

    println!("{} listening; addr={:?}", name, addr);

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
            ..Default::default()
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

use support::bytes::IntoBuf;
use support::hyper::body::Payload;
use support::*;
// use support::tokio::executor::Executor as _TokioExecutor;

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::ops::{Bound, RangeBounds};
use std::sync::{Arc, Mutex};

use linkerd2_proxy_api::destination as pb;
use linkerd2_proxy_api::net;

pub fn new() -> Controller {
    Controller::new()
}

pub fn identity() -> identity::Controller {
    identity::Controller::new()
}

pub type Labels = HashMap<String, String>;

#[derive(Debug)]
pub struct DstReceiver(sync::mpsc::UnboundedReceiver<Result<pb::Update, grpc::Status>>);

#[derive(Clone, Debug)]
pub struct DstSender(sync::mpsc::UnboundedSender<Result<pb::Update, grpc::Status>>);

#[derive(Debug)]
pub struct ProfileReceiver(sync::mpsc::UnboundedReceiver<pb::DestinationProfile>);

#[derive(Clone, Debug)]
pub struct ProfileSender(sync::mpsc::UnboundedSender<pb::DestinationProfile>);

#[derive(Clone, Debug, Default)]
pub struct Controller {
    expect_dst_calls: Arc<Mutex<VecDeque<Dst>>>,
    expect_profile_calls: Arc<Mutex<VecDeque<(pb::GetDestination, ProfileReceiver)>>>,
}

pub struct Listening {
    pub addr: SocketAddr,
    shutdown: Shutdown,
}

#[derive(Clone, Debug, Default)]
pub struct RouteBuilder {
    route: pb::Route,
}

#[derive(Debug)]
enum Dst {
    Call(pb::GetDestination, DstReceiver),
    Done,
}

impl Controller {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn destination_tx(&self, dest: &str) -> DstSender {
        let (tx, rx) = sync::mpsc::unbounded();
        let path = if dest.contains(":") {
            dest.to_owned()
        } else {
            format!("{}:80", dest)
        };
        let dst = pb::GetDestination {
            scheme: "k8s".into(),
            path,
            ..Default::default()
        };
        self.expect_dst_calls
            .lock()
            .unwrap()
            .push_back(Dst::Call(dst, DstReceiver(rx)));
        DstSender(tx)
    }

    pub fn destination_err(self, dest: &str, err: grpc::Code) -> Self {
        self.destination_tx(dest)
            .send_err(grpc::Status::new(err, "unit test controller fake error"));
        self
    }

    pub fn destination_and_close(self, dest: &str, addr: SocketAddr) -> Self {
        self.destination_tx(dest).send_addr(addr);
        self
    }

    pub fn destination_close(self, dest: &str) -> Self {
        drop(self.destination_tx(dest));
        self
    }

    pub fn no_more_destinations(self) -> Self {
        self.expect_dst_calls.lock().unwrap().push_back(Dst::Done);
        self
    }

    pub fn delay_listen<F>(self, f: F) -> Listening
    where
        F: Future<Item = (), Error = ()> + Send + 'static,
    {
        run(
            pb::server::DestinationServer::new(self),
            "support destination service",
            Some(Box::new(f.then(|_| Ok(())))),
        )
    }

    pub fn profile_tx(&self, dest: &str) -> ProfileSender {
        let (tx, rx) = sync::mpsc::unbounded();
        let path = if dest.contains(":") {
            dest.to_owned()
        } else {
            format!("{}:80", dest)
        };
        let dst = pb::GetDestination {
            scheme: "k8s".into(),
            path,
            ..Default::default()
        };
        self.expect_profile_calls
            .lock()
            .unwrap()
            .push_back((dst, ProfileReceiver(rx)));
        ProfileSender(tx)
    }

    pub fn run(self) -> Listening {
        run(
            pb::server::DestinationServer::new(self),
            "support destination service",
            None,
        )
    }
}

fn grpc_internal_code() -> grpc::Status {
    grpc::Status::new(grpc::Code::Internal, "unit test controller internal error")
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

impl Stream for DstReceiver {
    type Item = pb::Update;
    type Error = grpc::Status;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match try_ready!(self.0.poll().map_err(|_| grpc_internal_code())) {
            Some(res) => Ok(Async::Ready(Some(res?))),
            None => Ok(Async::Ready(None)),
        }
    }
}

impl DstSender {
    pub fn send(&self, up: pb::Update) {
        self.0.unbounded_send(Ok(up)).expect("send dst update")
    }

    pub fn send_err(&self, e: grpc::Status) {
        self.0.unbounded_send(Err(e)).expect("send dst err")
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
}

impl Stream for ProfileReceiver {
    type Item = pb::DestinationProfile;
    type Error = grpc::Status;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.0.poll().map_err(|_| grpc_internal_code())
    }
}

impl ProfileSender {
    pub fn send(&self, up: pb::DestinationProfile) {
        self.0.unbounded_send(up).expect("send profile update")
    }
}

impl pb::server::Destination for Controller {
    type GetStream = DstReceiver;
    type GetFuture = future::FutureResult<grpc::Response<Self::GetStream>, grpc::Status>;

    fn get(&mut self, req: grpc::Request<pb::GetDestination>) -> Self::GetFuture {
        if let Ok(mut calls) = self.expect_dst_calls.lock() {
            match calls.pop_front() {
                Some(Dst::Call(dst, updates)) => {
                    if &dst == req.get_ref() {
                        return future::ok(grpc::Response::new(updates));
                    }

                    calls.push_front(Dst::Call(dst, updates));
                    return future::err(grpc_unexpected_request());
                }
                Some(Dst::Done) => {
                    panic!("unit test controller expects no more Destination.Get calls")
                }
                _ => {}
            }
        }

        future::err(grpc_no_results())
    }

    type GetProfileStream = ProfileReceiver;
    type GetProfileFuture =
        future::FutureResult<grpc::Response<Self::GetProfileStream>, grpc::Status>;

    fn get_profile(&mut self, req: grpc::Request<pb::GetDestination>) -> Self::GetProfileFuture {
        if let Ok(mut calls) = self.expect_profile_calls.lock() {
            if let Some((dst, profile)) = calls.pop_front() {
                if &dst == req.get_ref() {
                    return future::ok(grpc::Response::new(profile));
                }

                calls.push_front((dst, profile));
                return future::err(grpc_unexpected_request());
            }
        }

        future::err(grpc_no_results())
    }
}

pub(in support) fn run<T, B>(
    svc: T,
    name: &'static str,
    delay: Option<Box<Future<Item = (), Error = ()> + Send>>,
) -> Listening
where
    T: Service<http::Request<tower_grpc::BoxBody>, Response = http::Response<B>>,
    T: Clone + Send + Sync + 'static,
    T::Error: ::std::error::Error + Send + Sync,
    T::Future: Send,
    B: grpc::Body + Send + 'static,
    B::Data: Send + 'static,
{
    let (tx, rx) = shutdown_signal();

    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let listener = net2::TcpBuilder::new_v4().expect("Tcp::new_v4");
    listener.bind(addr).expect("Tcp::bind");
    let addr = listener.local_addr().expect("Tcp::local_addr");

    let (listening_tx, listening_rx) = oneshot::channel();
    let mut listening_tx = Some(listening_tx);

    ::std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            if let Some(delay) = delay {
                let _ = listening_tx.take().unwrap().send(());
                delay.wait().expect("support server delay wait");
            }
            let mut runtime = runtime::current_thread::Runtime::new().expect("support runtime");

            let listener = listener.listen(1024).expect("Tcp::listen");
            let bind =
                TcpListener::from_std(listener, &reactor::Handle::default()).expect("from_std");

            if let Some(listening_tx) = listening_tx {
                let _ = listening_tx.send(());
            }

            let name = name.clone();
            let serve = hyper::Server::builder(bind.incoming())
                .http2_only(true)
                .serve(move || {
                    let svc = Mutex::new(svc.clone());
                    hyper::service::service_fn(move |req| {
                        let req = req.map(|body| tower_grpc::BoxBody::map_from(body));
                        svc.lock()
                            .expect("svc lock")
                            .call(req)
                            .map(|res| res.map(GrpcToPayload))
                    })
                })
                .map_err(move |e| println!("{} error: {:?}", name, e));

            runtime.spawn(serve);
            runtime.block_on(rx).expect(name);
        })
        .unwrap();

    listening_rx.wait().expect("listening_rx");
    println!("{} listening; addr={:?}", name, addr);

    Listening { addr, shutdown: tx }
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

pub fn profile<I>(routes: I, retry_budget: Option<pb::RetryBudget>) -> pb::DestinationProfile
where
    I: IntoIterator,
    I::Item: Into<pb::Route>,
{
    let routes = routes.into_iter().map(Into::into).collect();
    pb::DestinationProfile {
        routes,
        retry_budget,
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

struct GrpcToPayload<B>(B);

impl<B> Payload for GrpcToPayload<B>
where
    B: tower_grpc::Body + Send + 'static,
    B::Data: Send + 'static,
    <B::Data as IntoBuf>::Buf: Send + 'static,
{
    type Data = <B::Data as IntoBuf>::Buf;
    type Error = B::Error;

    fn is_end_stream(&self) -> bool {
        self.0.is_end_stream()
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        let data = try_ready!(self.0.poll_data());
        Ok(data.map(IntoBuf::into_buf).into())
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        self.0.poll_trailers()
    }
}

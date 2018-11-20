#![cfg_attr(feature = "cargo-clippy", allow(clone_on_ref_ptr))]

use support::*;
use support::bytes::IntoBuf;
use support::hyper::body::Payload;
// use support::tokio::executor::Executor as _TokioExecutor;

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use linkerd2_proxy_api::destination as pb;
use linkerd2_proxy_api::net;

pub fn new() -> Controller {
    Controller::new()
}

pub type Labels = HashMap<String, String>;

#[derive(Debug)]
pub struct DstReceiver(sync::mpsc::UnboundedReceiver<pb::Update>);

#[derive(Clone, Debug)]
pub struct DstSender(sync::mpsc::UnboundedSender<pb::Update>);

#[derive(Debug)]
pub struct ProfileReceiver(sync::mpsc::UnboundedReceiver<pb::DestinationProfile>);

#[derive(Clone, Debug)]
pub struct ProfileSender(sync::mpsc::UnboundedSender<pb::DestinationProfile>);

#[derive(Clone, Debug, Default)]
pub struct Controller {
    expect_dst_calls: Arc<Mutex<VecDeque<(pb::GetDestination, DstReceiver)>>>,
    expect_profile_calls: Arc<Mutex<VecDeque<(pb::GetDestination, ProfileReceiver)>>>,
}

pub struct Listening {
    pub addr: SocketAddr,
    shutdown: Shutdown,
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
        };
        self.expect_dst_calls
            .lock()
            .unwrap()
            .push_back((dst, DstReceiver(rx)));
        DstSender(tx)
    }

    pub fn destination_and_close(self, dest: &str, addr: SocketAddr) -> Self {
        self.destination_tx(dest).send_addr(addr);
        self
    }

    pub fn destination_close(self, dest: &str) -> Self {
        drop(self.destination_tx(dest));
        self
    }

    pub fn delay_listen<F>(self, f: F) -> Listening
    where
        F: Future<Item=(), Error=()> + Send + 'static,
    {
        run(self, Some(Box::new(f.then(|_| Ok(())))))
    }

    pub fn profile_tx(&self, dest: &str) -> ProfileSender {
        let (tx, rx) = sync::mpsc::unbounded();
        let dst = pb::GetDestination {
            scheme: "k8s".into(),
            path: dest.into(),
        };
        self.expect_profile_calls
            .lock()
            .unwrap()
            .push_back((dst, ProfileReceiver(rx)));
        ProfileSender(tx)
    }

    pub fn run(self) -> Listening {
        run(self, None)
    }
}

fn grpc_internal_code() -> grpc::Error {
    grpc::Error::Grpc(grpc::Status::with_code(grpc::Code::Internal), HeaderMap::new())
}

impl Stream for DstReceiver {
    type Item = pb::Update;
    type Error = grpc::Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.0.poll().map_err(|_| grpc_internal_code())
    }
}

impl DstSender {
    pub fn send(&self, up: pb::Update) {
        self.0.unbounded_send(up).expect("send dst update")
    }

    pub fn send_addr(&self, addr: SocketAddr) {
        self.send(destination_add(addr))
    }

    pub fn send_labeled(&self, addr: SocketAddr, addr_labels: Labels, parent_labels: Labels) {
        self.send(destination_add_labeled(addr, Hint::Unknown, addr_labels, parent_labels));
    }

    pub fn send_h2_hinted(&self, addr: SocketAddr) {
        self.send(destination_add_hinted(addr, Hint::H2));
    }
}

impl Stream for ProfileReceiver {
    type Item = pb::DestinationProfile;
    type Error = grpc::Error;
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
    type GetFuture = future::FutureResult<grpc::Response<Self::GetStream>, grpc::Error>;

    fn get(&mut self, req: grpc::Request<pb::GetDestination>) -> Self::GetFuture {
        if let Ok(mut calls) = self.expect_dst_calls.lock() {
            if let Some((dst, updates)) = calls.pop_front() {
                if &dst == req.get_ref() {
                    return future::ok(grpc::Response::new(updates));
                }

                calls.push_front((dst, updates));
            }
        }

        future::err(grpc_internal_code())
    }

    type GetProfileStream = ProfileReceiver;
    type GetProfileFuture = future::FutureResult<grpc::Response<Self::GetProfileStream>, grpc::Error>;

    fn get_profile(&mut self, req: grpc::Request<pb::GetDestination>) -> Self::GetProfileFuture {
        if let Ok(mut calls) = self.expect_profile_calls.lock() {
            if let Some((dst, profile)) = calls.pop_front() {
                if &dst == req.get_ref() {
                    return future::ok(grpc::Response::new(profile));
                }

                calls.push_front((dst, profile));
            }
        }

        future::err(grpc_internal_code())
    }
}

fn run(controller: Controller, delay: Option<Box<Future<Item=(), Error=()> + Send>>) -> Listening {
    let (tx, rx) = shutdown_signal();

    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let listener = net2::TcpBuilder::new_v4().expect("Tcp::new_v4");
    listener.bind(addr).expect("Tcp::bind");
    let addr = listener.local_addr().expect("Tcp::local_addr");

    let (listening_tx, listening_rx) = oneshot::channel();
    let mut listening_tx = Some(listening_tx);

    ::std::thread::Builder::new()
        .name("support controller".into())
        .spawn(move || {
            if let Some(delay) = delay {
                let _ = listening_tx.take().unwrap().send(());
                delay.wait().expect("support server delay wait");
            }
            let dst_svc = pb::server::DestinationServer::new(controller);
            let mut runtime = runtime::current_thread::Runtime::new()
                .expect("support controller runtime");

            let listener = listener.listen(1024).expect("Tcp::listen");
            let bind = TcpListener::from_std(
                listener,
                &reactor::Handle::current()
            ).expect("from_std");

            if let Some(listening_tx) = listening_tx {
                let _ = listening_tx.send(());
            }

            let serve = hyper::Server::builder(bind.incoming())
                .http2_only(true)
                .serve(move || {
                    let dst_svc = Mutex::new(dst_svc.clone());
                    hyper::service::service_fn(move |req| {
                        let req = req.map(|body| {
                            tower_grpc::BoxBody::new(Box::new(PayloadToGrpc(body)))
                        });
                        dst_svc
                            .lock()
                            .expect("dst_svc lock")
                            .call(req)
                            .map(|res| {
                                res.map(GrpcToPayload)
                            })
                    })
                })
                .map_err(|e| println!("controller error: {:?}", e));

            runtime.spawn(serve);
            runtime.block_on(rx).expect("support controller run");
        }).unwrap();

    listening_rx.wait().expect("listening_rx");

    Listening {
        addr,
        shutdown: tx,
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
    addr_labels: HashMap<String, String>)
    -> pb::Update
{
    let protocol_hint = match hint {
        Hint::Unknown => None,
        Hint::H2 => Some(pb::ProtocolHint {
            protocol: Some(pb::protocol_hint::Protocol::H2(pb::protocol_hint::H2 {})),
        }),
    };
    pb::Update {
        update: Some(pb::update::Update::Add(
            pb::WeightedAddrSet {
                addrs: vec![
                    pb::WeightedAddr {
                        addr: Some(net::TcpAddress {
                            ip: Some(ip_conv(addr.ip())),
                            port: u32::from(addr.port()),
                        }),
                        weight: 0,
                        metric_labels: addr_labels,
                        protocol_hint,
                        ..Default::default()
                    },
                ],
                metric_labels: set_labels,
            },
        )),
    }
}

pub fn destination_add_none() -> pb::Update {
    pb::Update {
        update: Some(pb::update::Update::Add(
            pb::WeightedAddrSet {
                addrs: Vec::new(),
                ..Default::default()
            },
        )),
    }
}

pub fn destination_remove_none() -> pb::Update {
    pb::Update {
        update: Some(pb::update::Update::Remove(
            pb::AddrSet {
                addrs: Vec::new(),
                ..Default::default()
            },
        )),
    }
}

pub fn destination_exists_with_no_endpoints() -> pb::Update {
    pb::Update {
        update: Some(pb::update::Update::NoEndpoints(
            pb::NoEndpoints { exists: true }
        )),
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
                ip: Some(net::ip_address::Ip::Ipv6(net::IPv6 {
                    first,
                    last,
                })),
            }
        }
    }
}

fn octets_to_u64s(octets: [u8; 16]) -> (u64, u64) {
    let first = (u64::from(octets[0]) << 56) + (u64::from(octets[1]) << 48)
        + (u64::from(octets[2]) << 40) + (u64::from(octets[3]) << 32)
        + (u64::from(octets[4]) << 24) + (u64::from(octets[5]) << 16)
        + (u64::from(octets[6]) << 8) + u64::from(octets[7]);
    let last = (u64::from(octets[8]) << 56) + (u64::from(octets[9]) << 48)
        + (u64::from(octets[10]) << 40) + (u64::from(octets[11]) << 32)
        + (u64::from(octets[12]) << 24) + (u64::from(octets[13]) << 16)
        + (u64::from(octets[14]) << 8) + u64::from(octets[15]);
    (first, last)
}

struct PayloadToGrpc(hyper::Body);

impl tower_grpc::Body for PayloadToGrpc {
    type Data = Bytes;

    fn is_end_stream(&self) -> bool {
        self.0.is_end_stream()
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, tower_grpc::Error> {
        let data = try_ready!(self.0.poll_data().map_err(|_| tower_grpc::Error::Inner(())));
        Ok(data.map(Bytes::from).into())
    }

    fn poll_metadata(&mut self) -> Poll<Option<http::HeaderMap>, tower_grpc::Error> {
        self.0.poll_trailers().map_err(|_| tower_grpc::Error::Inner(()))
    }
}

struct GrpcToPayload<B>(B);

impl<B> Payload for GrpcToPayload<B>
where
    B: tower_grpc::Body + Send + 'static,
    B::Data: Send + 'static,
    <B::Data as IntoBuf>::Buf: Send + 'static,
{
    type Data = <B::Data as IntoBuf>::Buf;
    type Error = tower_grpc::Error;

    fn is_end_stream(&self) -> bool {
        self.0.is_end_stream()
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, tower_grpc::Error> {
        let data = try_ready!(self.0.poll_data());
        Ok(data.map(IntoBuf::into_buf).into())
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, tower_grpc::Error> {
        self.0.poll_metadata()
    }
}



#![cfg_attr(feature = "cargo-clippy", allow(clone_on_ref_ptr))]

use support::*;
use support::futures::future::Executor;
// use support::tokio::executor::Executor as _TokioExecutor;

use std::collections::{HashMap, VecDeque};
use std::io;
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

#[derive(Clone, Debug, Default)]
pub struct Controller {
    expect_dst_calls: Arc<Mutex<VecDeque<(pb::GetDestination, DstReceiver)>>>,
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
        let dst = pb::GetDestination {
            scheme: "k8s".into(),
            path: dest.into(),
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

    pub fn run(self) -> Listening {
        run(self, None)
    }
}

impl Stream for DstReceiver {
    type Item = pb::Update;
    type Error = grpc::Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.0.poll().map_err(|_| grpc::Error::Grpc(grpc::Status::INTERNAL, HeaderMap::new()))
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

        future::err(grpc::Error::Grpc(grpc::Status::INTERNAL, HeaderMap::new()))
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
            let new = pb::server::DestinationServer::new(controller);
            let mut runtime = runtime::current_thread::Runtime::new()
                .expect("support controller runtime");
            let h2 = tower_h2::Server::new(new,
                Default::default(),
                LazyExecutor,
            );

            let listener = listener.listen(1024).expect("Tcp::listen");
            let bind = TcpListener::from_std(
                listener,
                &reactor::Handle::current()
            ).expect("from_std");

            if let Some(listening_tx) = listening_tx {
                let _ = listening_tx.send(());
            }

            let serve = bind.incoming()
                .fold(h2, |h2, sock| {
                    if let Err(e) = sock.set_nodelay(true) {
                        return Err(e);
                    }

                    let serve = h2.serve(sock);
                    current_thread::TaskExecutor::current()
                        .execute(serve.map_err(|e| println!("controller error: {:?}", e)))
                        .map_err(|e| {
                            println!("controller execute error: {:?}", e);
                            io::Error::from(io::ErrorKind::Other)
                        })
                        .map(|_| h2)
                });


            runtime.spawn(Box::new(
                serve
                    .map(|_| ())
                    .map_err(|e| println!("controller error: {}", e)),
            ));
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

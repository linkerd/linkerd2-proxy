use super::{Outbound, ParentRef, Routes};
use crate::test_util::*;
use linkerd_app_core::{
    io,
    svc::{self, NewService},
    transport::addrs::*,
    Result,
};
use linkerd_app_test::{AsyncReadExt, AsyncWriteExt};
use linkerd_proxy_client_policy::{self as client_policy, tls::sni};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::watch;
use tokio_rustls::rustls::pki_types::DnsName;

mod basic;

const REQUEST: &[u8] = b"who r u?";
type Reponse = tokio::task::JoinHandle<io::Result<String>>;

#[derive(Clone, Debug)]
struct Target {
    num: usize,
    routes: watch::Receiver<Routes>,
}

#[derive(Clone, Debug)]

struct MockServer {
    io: support::io::Builder,
    addr: SocketAddr,
}

#[derive(Clone, Debug, Default)]
struct ConnectTcp {
    srvs: Arc<Mutex<HashMap<SocketAddr, MockServer>>>,
}

// === impl MockServer ===

impl MockServer {
    fn new(
        addr: SocketAddr,
        service_name: &str,
        client_hello: Vec<u8>,
    ) -> (Self, io::DuplexStream, Reponse) {
        let mut io = support::io();

        io.write(&client_hello)
            .write(REQUEST)
            .read(service_name.as_bytes());

        let server = MockServer { io, addr };
        let (io, response) = spawn_io(client_hello);

        (server, io, response)
    }
}

// === impl Target ===

impl PartialEq for Target {
    fn eq(&self, other: &Self) -> bool {
        self.num == other.num
    }
}

impl Eq for Target {}

impl std::hash::Hash for Target {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.num.hash(state);
    }
}

impl svc::Param<watch::Receiver<Routes>> for Target {
    fn param(&self) -> watch::Receiver<Routes> {
        self.routes.clone()
    }
}

// === impl ConnectTcp ===

impl ConnectTcp {
    fn add_server(&mut self, s: MockServer) {
        self.srvs.lock().insert(s.addr, s);
    }
}

impl<T: svc::Param<Remote<ServerAddr>>> svc::Service<T> for ConnectTcp {
    type Response = (support::io::Mock, Local<ClientAddr>);
    type Error = io::Error;
    type Future = future::Ready<io::Result<Self::Response>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, t: T) -> Self::Future {
        let Remote(ServerAddr(addr)) = t.param();
        let mut mock = self
            .srvs
            .lock()
            .remove(&addr)
            .expect("tried to connect to an unexpected address");

        assert_eq!(addr, mock.addr);
        let local = Local(ClientAddr(addr));
        future::ok::<_, support::io::Error>((mock.io.build(), local))
    }
}

fn spawn_io(
    client_hello: Vec<u8>,
) -> (
    io::DuplexStream,
    tokio::task::JoinHandle<io::Result<String>>,
) {
    let (mut client_io, server_io) = io::duplex(100);
    let task = tokio::spawn(async move {
        client_io.write_all(&client_hello).await?;
        client_io.write_all(REQUEST).await?;

        let mut buf = String::with_capacity(100);
        client_io.read_to_string(&mut buf).await?;
        Ok(buf)
    });
    (server_io, task)
}

fn default_backend(addr: SocketAddr) -> client_policy::Backend {
    use client_policy::{Backend, BackendDispatcher, EndpointMetadata, Meta, Queue};
    Backend {
        meta: Meta::new_default("test"),
        queue: Queue {
            capacity: 100,
            failfast_timeout: Duration::from_secs(10),
        },
        dispatcher: BackendDispatcher::Forward(addr, EndpointMetadata::default()),
    }
}

fn sni_route(backend: client_policy::Backend, sni: sni::MatchSni) -> client_policy::tls::Route {
    use client_policy::{
        tls::{Filter, Policy, Route},
        Meta, RouteBackend, RouteDistribution,
    };
    use once_cell::sync::Lazy;
    static NO_FILTERS: Lazy<Arc<[Filter]>> = Lazy::new(|| Arc::new([]));
    Route {
        snis: vec![sni],
        policy: Policy {
            meta: Meta::new_default("test_route"),
            filters: NO_FILTERS.clone(),
            params: (),
            distribution: RouteDistribution::FirstAvailable(Arc::new([RouteBackend {
                filters: NO_FILTERS.clone(),
                backend,
            }])),
        },
    }
}

// generates a sample ClientHello TLS message for testing
fn generate_client_hello(sni: &str) -> Vec<u8> {
    use tokio_rustls::rustls::{
        internal::msgs::{
            base::Payload,
            codec::{Codec, Reader},
            enums::Compression,
            handshake::{
                ClientExtension, ClientHelloPayload, HandshakeMessagePayload, HandshakePayload,
                Random, ServerName, SessionId,
            },
            message::{MessagePayload, PlainMessage},
        },
        CipherSuite, ContentType, HandshakeType, ProtocolVersion,
    };

    let sni = DnsName::try_from(sni.to_string()).unwrap();
    let sni = trim_hostname_trailing_dot_for_sni(&sni);

    let mut server_name_bytes = vec![];
    0u8.encode(&mut server_name_bytes); // encode the type first
    (sni.as_ref().len() as u16).encode(&mut server_name_bytes); // then the length as u16
    server_name_bytes.extend_from_slice(sni.as_ref().as_bytes()); // then the server name itself

    let server_name =
        ServerName::read(&mut Reader::init(&server_name_bytes)).expect("Server name is valid");

    let hs_payload = HandshakeMessagePayload {
        typ: HandshakeType::ClientHello,
        payload: HandshakePayload::ClientHello(ClientHelloPayload {
            client_version: ProtocolVersion::TLSv1_2,
            random: Random::from([0; 32]),
            session_id: SessionId::read(&mut Reader::init(&[0])).unwrap(),
            cipher_suites: vec![CipherSuite::TLS_NULL_WITH_NULL_NULL],
            compression_methods: vec![Compression::Null],
            extensions: vec![ClientExtension::ServerName(vec![server_name])],
        }),
    };

    let mut hs_payload_bytes = Vec::default();
    MessagePayload::handshake(hs_payload).encode(&mut hs_payload_bytes);

    let message = PlainMessage {
        typ: ContentType::Handshake,
        version: ProtocolVersion::TLSv1_2,
        payload: Payload::Owned(hs_payload_bytes),
    };

    message.into_unencrypted_opaque().encode()
}

fn trim_hostname_trailing_dot_for_sni(dns_name: &DnsName<'_>) -> DnsName<'static> {
    let dns_name_str = dns_name.as_ref();

    // RFC6066: "The hostname is represented as a byte string using
    // ASCII encoding without a trailing dot"
    if dns_name_str.ends_with('.') {
        let trimmed = &dns_name_str[0..dns_name_str.len() - 1];
        DnsName::try_from(trimmed).unwrap().to_owned()
    } else {
        dns_name.to_owned()
    }
}

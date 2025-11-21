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
use linkerd_rustls::tokio_rustls::rustls::{
    internal::msgs::codec::{Codec, Reader},
    pki_types::DnsName,
    InvalidMessage,
};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    marker::PhantomData,
    net::SocketAddr,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::watch;

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
        dispatcher: BackendDispatcher::Forward(addr, EndpointMetadata::default().into()),
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
            params: Default::default(),
            distribution: RouteDistribution::FirstAvailable(Arc::new([RouteBackend {
                filters: NO_FILTERS.clone(),
                backend,
            }])),
        },
    }
}

// generates a sample ClientHello TLS message for testing
fn generate_client_hello(sni: &str) -> Vec<u8> {
    use linkerd_rustls::tokio_rustls::rustls::{
        internal::msgs::{base::Payload, codec::Codec, message::PlainMessage},
        ContentType, ProtocolVersion,
    };

    let sni = DnsName::try_from(sni.to_string()).unwrap();
    let sni = trim_hostname_trailing_dot_for_sni(&sni);

    // rustls has internal-only types that can encode a ClientHello, but they are mostly
    // inaccessible and an unstable part of the public API anyway. Manually encode one here for
    // testing only instead.

    let mut hs_payload_bytes = vec![];
    1u8.encode(&mut hs_payload_bytes); // client hello ID

    let client_hello_body = {
        let mut payload = LengthPayload::<U24>::empty();

        payload.buf.extend_from_slice(&[0x03, 0x03]); // client version, TLSv1.2

        payload.buf.extend_from_slice(&[0u8; 32]); // random

        0u8.encode(&mut payload.buf); // session ID

        LengthPayload::<u16>::from_slice(&[0x00, 0x00] /* TLS_NULL_WITH_NULL_NULL */)
            .encode(&mut payload.buf);

        LengthPayload::<u8>::from_slice(&[0x00] /* no compression */).encode(&mut payload.buf);

        let extensions = {
            let mut payload = LengthPayload::<u16>::empty();
            0u16.encode(&mut payload.buf); // server name extension ID

            let server_name_extension = {
                let mut payload = LengthPayload::<u16>::empty();
                let server_name = {
                    let mut payload = LengthPayload::<u16>::empty();
                    0u8.encode(&mut payload.buf); // DNS hostname ID
                    LengthPayload::<u16>::from_slice(sni.as_ref().as_bytes())
                        .encode(&mut payload.buf);
                    payload
                };
                server_name.encode(&mut payload.buf);
                payload
            };
            server_name_extension.encode(&mut payload.buf);
            payload
        };
        extensions.encode(&mut payload.buf);
        payload
    };
    client_hello_body.encode(&mut hs_payload_bytes);

    let message = PlainMessage {
        typ: ContentType::Handshake,
        version: ProtocolVersion::TLSv1_2,
        payload: Payload::Owned(hs_payload_bytes),
    };

    message.into_unencrypted_opaque().encode()
}

#[derive(Debug)]
struct LengthPayload<T> {
    buf: Vec<u8>,
    _boo: PhantomData<fn() -> T>,
}

impl<T> LengthPayload<T> {
    fn empty() -> Self {
        Self {
            buf: vec![],
            _boo: PhantomData,
        }
    }

    fn from_slice(s: &[u8]) -> Self {
        Self {
            buf: s.to_vec(),
            _boo: PhantomData,
        }
    }
}

impl Codec<'_> for LengthPayload<u8> {
    fn encode(&self, bytes: &mut Vec<u8>) {
        (self.buf.len() as u8).encode(bytes);
        bytes.extend_from_slice(&self.buf);
    }

    fn read(_: &mut Reader<'_>) -> std::result::Result<Self, InvalidMessage> {
        unimplemented!()
    }
}

impl Codec<'_> for LengthPayload<u16> {
    fn encode(&self, bytes: &mut Vec<u8>) {
        (self.buf.len() as u16).encode(bytes);
        bytes.extend_from_slice(&self.buf);
    }

    fn read(_: &mut Reader<'_>) -> std::result::Result<Self, InvalidMessage> {
        unimplemented!()
    }
}

#[derive(Debug)]
struct U24;

impl Codec<'_> for LengthPayload<U24> {
    fn encode(&self, bytes: &mut Vec<u8>) {
        let len = self.buf.len() as u32;
        bytes.extend_from_slice(&len.to_be_bytes()[1..]);
        bytes.extend_from_slice(&self.buf);
    }

    fn read(_: &mut Reader<'_>) -> std::result::Result<Self, InvalidMessage> {
        unimplemented!()
    }
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

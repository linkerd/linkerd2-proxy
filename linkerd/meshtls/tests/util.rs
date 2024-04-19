#![deny(warnings, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use futures::prelude::*;
use linkerd_conditional::Conditional;
use linkerd_dns_name::Name;
use linkerd_error::Infallible;
use linkerd_identity::{Credentials, DerX509, Id};
use linkerd_io::{self as io, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use linkerd_meshtls as meshtls;
use linkerd_proxy_transport::{
    addrs::*,
    listen::{Addrs, Bind, BindTcp},
    ConnectTcp, Keepalive,
};
use linkerd_stack::{
    layer::Layer, service_fn, ExtractParam, InsertParam, NewService, Param, ServiceExt,
};
use linkerd_tls as tls;
use linkerd_tls_test_util as test_util;
use rcgen::{BasicConstraints, Certificate, CertificateParams, IsCa, SanType};
use std::str::FromStr;
use std::{
    net::SocketAddr,
    sync::mpsc,
    time::{Duration, SystemTime},
};
use tokio::net::TcpStream;
use tracing::Instrument;

fn generate_cert_with_name(subject_alt_names: Vec<SanType>) -> (Vec<u8>, Vec<u8>, String) {
    let mut root_params = CertificateParams::default();
    root_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let root_cert = Certificate::from_params(root_params).expect("should generate root");

    let mut params = CertificateParams::default();
    params.subject_alt_names = subject_alt_names;

    let cert = Certificate::from_params(params).expect("should generate cert");

    (
        cert.serialize_der_with_signer(&root_cert)
            .expect("should serialize"),
        cert.serialize_private_key_der(),
        root_cert.serialize_pem().expect("should serialize"),
    )
}

pub fn fails_processing_cert_when_wrong_id_configured(mode: meshtls::Mode) {
    let server_name = Name::from_str("system.local").expect("should parse");
    let id = Id::Dns(server_name.clone());

    let (cert, key, roots) =
        generate_cert_with_name(vec![SanType::URI("spiffe://system/local".into())]);
    let (mut store, _) = mode
        .watch(id, server_name.clone(), &roots)
        .expect("should construct");

    let err = store
        .set_certificate(DerX509(cert), vec![], key, SystemTime::now())
        .expect_err("should error");

    assert_eq!(
        "certificate does not match TLS identity",
        format!("{}", err),
    );
}

pub async fn plaintext(mode: meshtls::Mode) {
    let (_foo, _, server_tls) = load(mode, &test_util::FOO_NS1);
    let (_bar, client_tls, _) = load(mode, &test_util::BAR_NS1);
    let (client_result, server_result) = run_test(
        client_tls,
        Conditional::None(tls::NoClientTls::NotProvidedByServiceDiscovery),
        |conn| write_then_read(conn, PING),
        server_tls,
        |(_, conn)| read_then_write(conn, PING.len(), PONG),
    )
    .await;
    assert_eq!(
        client_result.tls,
        Some(Conditional::None(
            tls::NoClientTls::NotProvidedByServiceDiscovery
        ))
    );
    assert_eq!(&client_result.result.expect("pong")[..], PONG);
    assert_eq!(
        server_result.tls,
        Some(Conditional::None(tls::NoServerTls::NoClientHello))
    );
    assert_eq!(&server_result.result.expect("ping")[..], PING);
}

pub async fn proxy_to_proxy_tls_works(mode: meshtls::Mode) {
    let (_foo, _, server_tls) = load(mode, &test_util::FOO_NS1);
    let (_bar, client_tls, _) = load(mode, &test_util::BAR_NS1);
    let server_id = tls::ServerId(test_util::FOO_NS1.id.parse().unwrap());
    let server_name = tls::ServerName(test_util::FOO_NS1.name.parse().unwrap());
    let (client_result, server_result) = run_test(
        client_tls.clone(),
        Conditional::Some(tls::ClientTls::new(server_id.clone(), server_name.clone())),
        |conn| write_then_read(conn, PING),
        server_tls,
        |(_, conn)| read_then_write(conn, PING.len(), PONG),
    )
    .await;
    assert_eq!(
        client_result.tls,
        Some(Conditional::Some(tls::ClientTls::new(
            server_id,
            server_name,
        )))
    );
    assert_eq!(&client_result.result.expect("pong")[..], PONG);
    assert_eq!(
        server_result.tls,
        Some(Conditional::Some(tls::ServerTls::Established {
            client_id: Some(tls::ClientId(test_util::BAR_NS1.name.parse().unwrap())),
            negotiated_protocol: None,
        }))
    );
    assert_eq!(&server_result.result.expect("ping")[..], PING);
}

pub async fn proxy_to_proxy_tls_pass_through_when_identity_does_not_match(mode: meshtls::Mode) {
    let (_foo, _, server_tls) = load(mode, &test_util::FOO_NS1);

    // Misuse the client's identity instead of the server's identity. Any
    // identity other than `server_tls.server_identity` would work.
    let (_bar, client_tls, _) = load(mode, &test_util::BAR_NS1);
    let server_id = test_util::BAR_NS1.id.parse::<tls::ServerId>().unwrap();
    let server_name = test_util::BAR_NS1.name.parse::<tls::ServerName>().unwrap();

    let (client_result, server_result) = run_test(
        client_tls,
        Conditional::Some(tls::ClientTls::new(server_id.clone(), server_name.clone())),
        |conn| write_then_read(conn, PING),
        server_tls,
        |(_, conn)| read_then_write(conn, START_OF_TLS.len(), PONG),
    )
    .await;

    // The server's connection will succeed with the TLS client hello passed
    // through, because the SNI doesn't match its identity.
    assert_eq!(client_result.tls, None);
    assert!(client_result.result.is_err());
    assert_eq!(
        server_result.tls,
        Some(Conditional::Some(tls::ServerTls::Passthru {
            sni: server_name
        }))
    );
    assert_eq!(&server_result.result.unwrap()[..], START_OF_TLS);
}

type ServerConn<T, I> = (
    (tls::ConditionalServerTls, T),
    io::EitherIo<meshtls::ServerIo<tls::server::DetectIo<I>>, tls::server::DetectIo<I>>,
);

fn load(
    mode: meshtls::Mode,
    ent: &test_util::Entity,
) -> (meshtls::creds::Store, meshtls::NewClient, meshtls::Server) {
    let roots_pem = std::str::from_utf8(ent.trust_anchors).expect("valid PEM");
    let (mut store, rx) = mode
        .watch(
            ent.name.parse().unwrap(),
            ent.name.parse().unwrap(),
            roots_pem,
        )
        .expect("credentials must be readable");

    store
        .set_certificate(
            DerX509(ent.crt.to_vec()),
            vec![],
            ent.key.to_vec(),
            SystemTime::now() + Duration::from_secs(1000),
        )
        .expect("certificate must be valid");

    (store, rx.new_client(), rx.server())
}

struct Transported<I, R> {
    tls: Option<I>,

    /// The connection's result.
    result: Result<R, io::Error>,
}

#[derive(Clone)]
struct ServerParams {
    identity: meshtls::Server,
}

type ClientIo =
    io::EitherIo<io::ScopedIo<TcpStream>, linkerd_meshtls::ClientIo<io::ScopedIo<TcpStream>>>;

/// Runs a test for a single TCP connection. `client` processes the connection
/// on the client side and `server` processes the connection on the server
/// side.
async fn run_test<C, CF, CR, S, SF, SR>(
    client_tls: meshtls::NewClient,
    client_server_id: Conditional<tls::ClientTls, tls::NoClientTls>,
    client: C,
    server_tls: meshtls::Server,
    server: S,
) -> (
    Transported<tls::ConditionalClientTls, CR>,
    Transported<tls::ConditionalServerTls, SR>,
)
where
    // Client
    C: FnOnce(ClientIo) -> CF + Clone + Send + 'static,
    CF: Future<Output = Result<CR, io::Error>> + Send + 'static,
    CR: Send + 'static,
    // Server
    S: Fn(ServerConn<Addrs, TcpStream>) -> SF + Clone + Send + 'static,
    SF: Future<Output = Result<SR, io::Error>> + Send + 'static,
    SR: Send + 'static,
{
    let _trace = linkerd_tracing::test::trace_init();

    // A future that will receive a single connection.
    let (server, server_addr, server_result) = {
        // Saves the result of every connection.
        let (sender, receiver) = mpsc::channel::<Transported<tls::ConditionalServerTls, SR>>();

        let detect = tls::NewDetectTls::<meshtls::Server, _, _>::new(
            ServerParams {
                identity: server_tls,
            },
            move |meta: (tls::ConditionalServerTls, Addrs)| {
                let server = server.clone();
                let sender = sender.clone();
                let tls = meta.0.clone().map(Into::into);
                service_fn(move |conn| {
                    let server = server.clone();
                    let sender = sender.clone();
                    let tls = Some(tls.clone());
                    let future = server((meta.clone(), conn));
                    Box::pin(
                        async move {
                            let result = future.await;
                            sender
                                .send(Transported { tls, result })
                                .expect("send result");
                            Ok::<(), Infallible>(())
                        }
                        .instrument(tracing::info_span!("test_svc")),
                    )
                })
            },
        );

        let (listen_addr, listen) = BindTcp::default().bind(&Server).expect("must bind");
        let server = async move {
            futures::pin_mut!(listen);
            let (addrs, io) = listen
                .next()
                .await
                .expect("listen failed")
                .expect("listener closed");
            tracing::debug!("incoming connection");
            let accept = detect.new_service(addrs);
            accept.oneshot(io).await.expect("connection failed");
            tracing::debug!("done");
        }
        .instrument(tracing::info_span!("run_server", %listen_addr));

        (server, listen_addr, receiver)
    };

    // A future that will open a single connection to the server.
    let (client, client_result) = {
        // Saves the result of the single connection. This could be a simpler
        // type, e.g. `Arc<Mutex>`, but using a channel simplifies the code and
        // parallels the server side.
        let (sender, receiver) = mpsc::channel::<Transported<tls::ConditionalClientTls, CR>>();

        let tls = Some(client_server_id.clone());
        let client = async move {
            let conn = tls::Client::layer(client_tls)
                .layer(ConnectTcp::new(Keepalive(None)))
                .oneshot(Target(server_addr.into(), client_server_id))
                .await;
            match conn {
                Err(e) => {
                    sender
                        .send(Transported {
                            tls: None,
                            result: Err(e),
                        })
                        .expect("send result");
                }
                Ok((conn, _)) => {
                    let result = client(conn).instrument(tracing::info_span!("client")).await;
                    sender
                        .send(Transported { tls, result })
                        .expect("send result");
                }
            };
        };

        (client, receiver)
    };

    futures::future::join(server, client).await;

    let client_result = client_result.try_recv().expect("client complete");

    // XXX: This assumes that only one connection is accepted. TODO: allow the
    // caller to observe the results for every connection, once we have tests
    // that allow accepting multiple connections.
    let server_result = server_result.try_recv().expect("server complete");

    (client_result, server_result)
}

/// Writes `to_write` and shuts down the write side, then reads until EOF,
/// returning the bytes read.
async fn write_then_read(
    conn: impl AsyncRead + AsyncWrite + Unpin,
    to_write: &'static [u8],
) -> Result<Vec<u8>, io::Error> {
    let conn = write_and_shutdown(conn, to_write).await;
    if let Err(ref write_err) = conn {
        tracing::error!(%write_err);
    }
    let mut conn = conn?;
    let mut vec = Vec::new();
    tracing::debug!("read_to_end");
    let read = conn.read_to_end(&mut vec).await;
    tracing::debug!(?read, ?vec);
    read?;
    Ok(vec)
}

/// Reads until EOF then writes `to_write` and shuts down the write side,
/// returning the bytes read.
async fn read_then_write(
    mut conn: impl AsyncRead + AsyncWrite + Unpin,
    read_prefix_len: usize,
    to_write: &'static [u8],
) -> Result<Vec<u8>, io::Error> {
    let mut vec = vec![0; read_prefix_len];
    tracing::debug!("read_exact");
    let read = conn.read_exact(&mut vec[..]).await;
    tracing::debug!(?read, ?vec);
    read?;
    write_and_shutdown(conn, to_write).await?;
    Ok(vec)
}

/// writes `to_write` to `conn` and then shuts down the write side of `conn`.
async fn write_and_shutdown<T: AsyncRead + AsyncWrite + Unpin>(
    mut conn: T,
    to_write: &'static [u8],
) -> Result<T, io::Error> {
    conn.write_all(to_write).await?;
    tracing::debug!("shutting down...");
    conn.shutdown().await?;
    tracing::debug!("shutdown done");
    Ok(conn)
}

const PING: &[u8] = b"ping";
const PONG: &[u8] = b"pong";
const START_OF_TLS: &[u8] = &[22, 3, 1]; // ContentType::handshake version 3.1

#[derive(Copy, Clone, Debug)]
struct Server;

#[derive(Clone)]
struct Target(SocketAddr, tls::ConditionalClientTls);

// === impl Target ===

impl Param<Remote<ServerAddr>> for Target {
    fn param(&self) -> Remote<ServerAddr> {
        Remote(ServerAddr(self.0))
    }
}

impl Param<tls::ConditionalClientTls> for Target {
    fn param(&self) -> tls::ConditionalClientTls {
        self.1.clone()
    }
}

// === impl Server ===

impl Param<ListenAddr> for Server {
    fn param(&self) -> ListenAddr {
        // Let the OS decide the port number and then return the resulting
        // `SocketAddr` so the client can connect to it. This allows multiple
        // tests to run at once, which wouldn't work if they all were bound on
        // a fixed port.
        ListenAddr(([127, 0, 0, 1], 0).into())
    }
}
impl Param<Keepalive> for Server {
    fn param(&self) -> Keepalive {
        Keepalive(None)
    }
}

// === impl ServerParams ===

impl<T> ExtractParam<tls::server::Timeout, T> for ServerParams {
    fn extract_param(&self, _: &T) -> tls::server::Timeout {
        tls::server::Timeout(Duration::from_secs(10))
    }
}

impl<T> ExtractParam<meshtls::Server, T> for ServerParams {
    fn extract_param(&self, _: &T) -> meshtls::Server {
        self.identity.clone()
    }
}

impl<T> InsertParam<tls::ConditionalServerTls, T> for ServerParams {
    type Target = (tls::ConditionalServerTls, T);

    #[inline]
    fn insert_param(&self, tls: tls::ConditionalServerTls, target: T) -> Self::Target {
        (tls, target)
    }
}

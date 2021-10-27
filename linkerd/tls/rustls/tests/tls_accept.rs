#![cfg(test)]

// These are basically integration tests for the `connection` submodule, but
// they cannot be "real" integration tests because `connection` isn't a public
// interface and because `connection` exposes a `#[cfg(test)]`-only API for use
// by these tests.

use futures::prelude::*;
use linkerd_conditional::Conditional;
use linkerd_identity as id;
use linkerd_io::{self as io, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use linkerd_proxy_transport::{
    addrs::*,
    listen::{Addrs, Bind, BindTcp},
    ConnectTcp, Keepalive, ListenAddr,
};
use linkerd_stack::{
    layer::Layer, service_fn, ExtractParam, InsertParam, NewService, Param, Service, ServiceExt,
};
use linkerd_tls as tls;
use linkerd_tls_rustls as rustls;
use linkerd_tls_test_util as test_util;
use std::{convert::Infallible, future::Future, net::SocketAddr, sync::mpsc, task, time::Duration};
use tokio::net::TcpStream;
use tracing::instrument::Instrument;

type ServerConn<T, I> = (
    (tls::ConditionalServerTls, T),
    io::EitherIo<rustls::ServerIo<tls::server::DetectIo<I>>, tls::server::DetectIo<I>>,
);

fn load(id: &linkerd_tls_test_util::Entity) -> (rustls::TrustAnchors, rustls::CrtKey) {
    let key = rustls::Key::from_pkcs8(id.key).expect("key must be valid");

    let crt = {
        let n = id.name.parse::<id::Name>().expect("name must be valid");
        let der = id.crt.iter().copied().collect();
        let expiry = std::time::SystemTime::now() + std::time::Duration::from_secs(60 * 60);
        rustls::Crt::new(id::LocalId(n), der, vec![], expiry)
    };

    let anchors = {
        let pem = std::str::from_utf8(id.trust_anchors).expect("utf-8");
        rustls::TrustAnchors::from_pem(pem).unwrap_or_else(rustls::TrustAnchors::empty)
    };

    let ck = anchors.certify(key, crt).expect("Identity must be valid");
    (anchors, ck)
}

#[tokio::test(flavor = "current_thread")]
async fn plaintext() {
    let (_, server_tls) = load(&test_util::FOO_NS1);
    let (_, client_tls) = load(&test_util::BAR_NS1);
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

#[tokio::test(flavor = "current_thread")]
async fn proxy_to_proxy_tls_works() {
    let (_, server_tls) = load(&test_util::FOO_NS1);
    let (_, client_tls) = load(&test_util::BAR_NS1);
    let server_id = tls::ServerId(server_tls.name().clone());
    let (client_result, server_result) = run_test(
        client_tls.clone(),
        Conditional::Some(server_id.clone()),
        |conn| write_then_read(conn, PING),
        server_tls,
        |(_, conn)| read_then_write(conn, PING.len(), PONG),
    )
    .await;
    assert_eq!(
        client_result.tls,
        Some(Conditional::Some(tls::ClientTls {
            server_id,
            alpn: None,
        }))
    );
    assert_eq!(&client_result.result.expect("pong")[..], PONG);
    assert_eq!(
        server_result.tls,
        Some(Conditional::Some(tls::ServerTls::Established {
            client_id: Some(tls::ClientId(client_tls.name().clone())),
            negotiated_protocol: None,
        }))
    );
    assert_eq!(&server_result.result.expect("ping")[..], PING);
}

#[tokio::test(flavor = "current_thread")]
async fn proxy_to_proxy_tls_pass_through_when_identity_does_not_match() {
    let (_, server_tls) = load(&test_util::FOO_NS1);

    // Misuse the client's identity instead of the server's identity. Any
    // identity other than `server_tls.server_identity` would work.
    let (_, client_tls) = load(&test_util::BAR_NS1);
    let sni = (**client_tls.id()).clone();

    let (client_result, server_result) = run_test(
        client_tls,
        Conditional::Some(tls::ServerId(sni.clone())),
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
            sni: tls::ServerId(sni)
        }))
    );
    assert_eq!(&server_result.result.unwrap()[..], START_OF_TLS);
}

struct Transported<I, R> {
    tls: Option<I>,

    /// The connection's result.
    result: Result<R, io::Error>,
}

#[derive(Clone)]
struct ServerParams {
    identity: rustls::CrtKey,
}

type ClientIo = io::EitherIo<io::ScopedIo<TcpStream>, rustls::ClientIo<io::ScopedIo<TcpStream>>>;

/// Runs a test for a single TCP connection. `client` processes the connection
/// on the client side and `server` processes the connection on the server
/// side.
async fn run_test<C, CF, CR, S, SF, SR>(
    client_tls: rustls::CrtKey,
    client_server_id: Conditional<tls::ServerId, tls::NoClientTls>,
    client: C,
    server_id: rustls::CrtKey,
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

        let detect = tls::NewDetectTls::<Tls, _, _>::new(
            ServerParams {
                identity: server_id,
            },
            move |meta: (tls::ConditionalServerTls, Addrs)| {
                let server = server.clone();
                let sender = sender.clone();
                let tls = Some(meta.0.clone().map(Into::into));
                service_fn(move |conn| {
                    let server = server.clone();
                    let sender = sender.clone();
                    let tls = tls.clone();
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
        let sender_clone = sender.clone();

        let tls = Some(client_server_id.clone().map(Into::into));
        let client = async move {
            let conn = tls::Client::layer(Tls(client_tls))
                .layer(ConnectTcp::new(Keepalive(None)))
                .oneshot(Target(server_addr.into(), client_server_id.map(Into::into)))
                .await;
            match conn {
                Err(e) => {
                    sender_clone
                        .send(Transported {
                            tls: None,
                            result: Err(e),
                        })
                        .expect("send result");
                }
                Ok(conn) => {
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

#[derive(Clone)]
struct Tls(rustls::CrtKey);

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

// === impl Tls ===

impl NewService<tls::ClientTls> for Tls {
    type Service = rustls::Connect;

    fn new_service(&self, target: tls::ClientTls) -> Self::Service {
        rustls::Connect::new(target, self.0.client_config())
    }
}

impl<I> Service<I> for Tls
where
    I: io::AsyncRead + io::AsyncWrite + Send + Unpin,
{
    type Response = (tls::ServerTls, rustls::ServerIo<I>);
    type Error = io::Error;
    type Future = rustls::TerminateFuture<I>;

    #[inline]
    fn poll_ready(&mut self, _: &mut task::Context<'_>) -> task::Poll<Result<(), io::Error>> {
        task::Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&mut self, io: I) -> Self::Future {
        rustls::terminate(self.0.server_config(), io)
    }
}

impl Param<tls::LocalId> for Tls {
    fn param(&self) -> tls::LocalId {
        self.0.id().clone()
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

/// === impl ServerParams ===

impl<T> ExtractParam<tls::server::Timeout, T> for ServerParams {
    fn extract_param(&self, _: &T) -> tls::server::Timeout {
        tls::server::Timeout(Duration::from_secs(10))
    }
}

impl<T> ExtractParam<Tls, T> for ServerParams {
    fn extract_param(&self, _: &T) -> Tls {
        Tls(self.identity.clone())
    }
}

impl<T> InsertParam<tls::ConditionalServerTls, T> for ServerParams {
    type Target = (tls::ConditionalServerTls, T);

    #[inline]
    fn insert_param(&self, tls: tls::ConditionalServerTls, target: T) -> Self::Target {
        (tls, target)
    }
}

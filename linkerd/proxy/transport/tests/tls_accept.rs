#![cfg(test)]
#![type_length_limit = "3659323"]
// These are basically integration tests for the `connection` submodule, but
// they cannot be "real" integration tests because `connection` isn't a public
// interface and because `connection` exposes a `#[cfg(test)]`-only API for use
// by these tests.

use futures::FutureExt;
use linkerd2_error::Never;
use linkerd2_identity::{test_util, CrtKey, Name};
use linkerd2_proxy_core::listen::{Accept, Bind as _Bind, Listen as CoreListen};
use linkerd2_proxy_transport::tls::{
    self,
    accept::{AcceptTls, Connection as ServerConnection},
    client::Connection as ClientConnection,
    Conditional,
};
use linkerd2_proxy_transport::{connect, Bind, Listen};
use std::future::Future;
use std::{net::SocketAddr, sync::mpsc};
use tokio::{
    self,
    io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
};
use tower::layer::Layer;
use tower::util::{service_fn, ServiceExt};
use tracing_futures::Instrument;

#[test]
fn plaintext() {
    let (client_result, server_result) = run_test(
        Conditional::None(tls::ReasonForNoIdentity::Disabled),
        |conn| write_then_read(conn, PING),
        Conditional::None(tls::ReasonForNoIdentity::Disabled),
        |(_, conn)| read_then_write(conn, PING.len(), PONG),
    );
    assert_eq!(client_result.is_tls(), false);
    assert_eq!(&client_result.result.expect("pong")[..], PONG);
    assert_eq!(server_result.is_tls(), false);
    assert_eq!(&server_result.result.expect("ping")[..], PING);
}

#[test]
fn proxy_to_proxy_tls_works() {
    let server_tls = test_util::FOO_NS1.validate().unwrap();
    let client_tls = test_util::BAR_NS1.validate().unwrap();
    let (client_result, server_result) = run_test(
        Conditional::Some((client_tls, server_tls.tls_server_name())),
        |conn| write_then_read(conn, PING),
        Conditional::Some(server_tls),
        |(_, conn)| read_then_write(conn, PING.len(), PONG),
    );
    assert_eq!(client_result.is_tls(), true);
    assert_eq!(&client_result.result.expect("pong")[..], PONG);
    assert_eq!(server_result.is_tls(), true);
    assert_eq!(&server_result.result.expect("ping")[..], PING);
}

#[test]
fn proxy_to_proxy_tls_pass_through_when_identity_does_not_match() {
    let server_tls = test_util::FOO_NS1.validate().unwrap();

    // Misuse the client's identity instead of the server's identity. Any
    // identity other than `server_tls.server_identity` would work.
    let client_tls = test_util::BAR_NS1.validate().expect("valid client cert");
    let client_target = test_util::BAR_NS1.crt().name().clone();

    let (client_result, server_result) = run_test(
        Conditional::Some((client_tls, client_target)),
        |conn| write_then_read(conn, PING),
        Conditional::Some(server_tls),
        |(_, conn)| read_then_write(conn, START_OF_TLS.len(), PONG),
    );

    // The server's connection will succeed with the TLS client hello passed
    // through, because the SNI doesn't match its identity.
    assert_eq!(client_result.is_tls(), false);
    assert!(client_result.result.is_err());
    assert_eq!(server_result.is_tls(), false);
    assert_eq!(&server_result.result.unwrap()[..], START_OF_TLS);
}

struct Transported<R> {
    /// The value of `Connection::peer_identity()` for the established connection.
    ///
    /// This will be `None` if we never even get a `Connection`.
    peer_identity: Option<tls::PeerIdentity>,

    /// The connection's result.
    result: Result<R, io::Error>,
}

impl<R> Transported<R> {
    fn is_tls(&self) -> bool {
        self.peer_identity
            .as_ref()
            .map(|i| i.is_some())
            .unwrap_or(false)
    }
}

/// Runs a test for a single TCP connection. `client` processes the connection
/// on the client side and `server` processes the connection on the server
/// side.
fn run_test<C, CF, CR, S, SF, SR>(
    client_tls: tls::Conditional<(CrtKey, Name)>,
    client: C,
    server_tls: tls::Conditional<CrtKey>,
    server: S,
) -> (Transported<CR>, Transported<SR>)
where
    // Client
    C: FnOnce(ClientConnection) -> CF + Clone + Send + 'static,
    CF: Future<Output = Result<CR, io::Error>> + Send + 'static,
    CR: Send + 'static,
    // Server
    S: Fn(ServerConnection) -> SF + Clone + Send + 'static,
    SF: Future<Output = Result<SR, io::Error>> + Send + 'static,
    SR: Send + 'static,
{
    {
        use tracing_subscriber::{fmt, EnvFilter};
        let sub = fmt::Subscriber::builder()
            .with_env_filter(EnvFilter::from_default_env())
            .finish();
        let _ = tracing::subscriber::set_global_default(sub);
    }

    let (client_tls, client_target_name) = match client_tls {
        Conditional::Some((crtkey, name)) => (
            Conditional::Some(ClientTls(crtkey)),
            Conditional::Some(name),
        ),
        Conditional::None(reason) => (Conditional::None(reason.clone()), Conditional::None(reason)),
    };

    // A future that will receive a single connection.
    let (server, server_addr, server_result) = {
        // Saves the result of every connection.
        let (sender, receiver) = mpsc::channel::<Transported<SR>>();

        // Let the OS decide the port number and then return the resulting
        // `SocketAddr` so the client can connect to it. This allows multiple
        // tests to run at once, which wouldn't work if they all were bound on
        // a fixed port.
        let addr = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
        let mut listen = Bind::new(addr, None).bind().expect("must bind");
        let listen_addr = listen.listen_addr();
        let mut accept = AcceptTls::new(
            server_tls,
            service_fn(move |(meta, conn): ServerConnection| {
                let server = server.clone();
                let sender = sender.clone();
                let peer_identity = Some(meta.peer_identity.clone());
                let future = server((meta, conn)).instrument(tracing::info_span!("test_svc"));
                futures::future::ok::<_, Never>(async move {
                    let result = future.await;
                    sender
                        .send(Transported {
                            peer_identity,
                            result,
                        })
                        .expect("send result");
                    Ok::<(), Never>(())
                })
            }),
        );
        let server = async move {
            let conn = futures::future::poll_fn(|cx| listen.poll_accept(cx))
                .await
                .expect("listen failed");
            tracing::debug!("incoming connection");
            let accept = accept.ready_and().await.expect("accept failed");
            tracing::debug!("accept ready");
            let res = accept.accept(conn).await.expect("connection failed").await;
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
        let (sender, receiver) = mpsc::channel::<Transported<CR>>();
        let sender_clone = sender.clone();

        let peer_identity = Some(client_target_name.clone());
        let client = async move {
            let conn = tls::ConnectLayer::new(client_tls)
                .layer(connect::Connect::new(None))
                .oneshot(Target(server_addr, client_target_name))
                .await;
            match conn {
                Err(e) => {
                    sender_clone
                        .send(Transported {
                            peer_identity: None,
                            result: Err(e),
                        })
                        .expect("send result");
                }
                Ok(conn) => {
                    let result = client(conn).instrument(tracing::info_span!("client")).await;
                    sender
                        .send(Transported {
                            peer_identity,
                            result,
                        })
                        .expect("send result");
                }
            };
        };

        (client, receiver)
    };

    tokio_compat::runtime::current_thread::run_std(
        futures::future::join(server, client).map(|_| ()),
    );

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
    mut conn: impl AsyncRead + AsyncWrite + Unpin,
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

#[derive(Clone)]
struct Target(SocketAddr, Conditional<Name>);

#[derive(Clone)]
struct ClientTls(CrtKey);

impl connect::ConnectAddr for Target {
    fn connect_addr(&self) -> SocketAddr {
        self.0
    }
}

impl tls::HasPeerIdentity for Target {
    fn peer_identity(&self) -> Conditional<Name> {
        self.1.clone()
    }
}

impl tls::client::HasConfig for ClientTls {
    fn tls_client_config(&self) -> std::sync::Arc<tls::client::Config> {
        self.0.tls_client_config()
    }
}

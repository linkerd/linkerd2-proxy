#![cfg(test)]

// These are basically integration tests for the `connection` submodule, but
// they cannot be "real" integration tests because `connection` isn't a public
// interface and because `connection` exposes a `#[cfg(test)]`-only API for use
// by these tests.

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
use std::{net::SocketAddr, sync::mpsc};
use tokio::{self, io, prelude::*};
use tower::{layer::Layer, ServiceExt};
use tower_util::service_fn;

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
    CF: Future<Item = CR, Error = io::Error> + Send + 'static,
    CR: Send + 'static,
    // Server
    S: Fn(ServerConnection) -> SF + Clone + Send + 'static,
    SF: Future<Item = SR, Error = io::Error> + Send + 'static,
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
        let listen = Bind::new(addr, None).bind().expect("must bind");
        let listen_addr = listen.listen_addr();

        let sender = service_fn(move |(meta, conn): ServerConnection| {
            let sender = sender.clone();
            let peer_identity = Some(meta.peer_identity.clone());

            let server = Box::new(server((meta, conn)).then(move |result| {
                sender
                    .send(Transported {
                        peer_identity,
                        result,
                    })
                    .expect("send result");

                future::ok::<(), Never>(())
            }));

            Box::new(future::ok::<_, Never>(server))
        });

        let accept = AcceptTls::new(server_tls, sender);
        let server = Server::Init { listen, accept };

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
        let client = tls::ConnectLayer::new(client_tls)
            .layer(connect::Connect::new(None))
            .oneshot(Target(server_addr, client_target_name))
            .map_err(move |e| {
                sender_clone
                    .send(Transported {
                        peer_identity: None,
                        result: Err(e),
                    })
                    .expect("send result");
                ()
            })
            .and_then(move |conn| {
                client(conn).then(move |result| {
                    sender
                        .send(Transported {
                            peer_identity,
                            result,
                        })
                        .expect("send result");
                    Ok(())
                })
            });

        (client, receiver)
    };

    tokio::run(server.join(client).map(|_| ()));

    let client_result = client_result.try_recv().expect("client complete");

    // XXX: This assumes that only one connection is accepted. TODO: allow the
    // caller to observe the results for every connection, once we have tests
    // that allow accepting multiple connections.
    let server_result = server_result.try_recv().expect("server complete");

    (client_result, server_result)
}

/// Writes `to_write` and shuts down the write side, then reads until EOF,
/// returning the bytes read.
fn write_then_read(
    conn: impl AsyncRead + AsyncWrite,
    to_write: &'static [u8],
) -> impl Future<Item = Vec<u8>, Error = io::Error> {
    write_and_shutdown(conn, to_write)
        .and_then(|conn| io::read_to_end(conn, Vec::new()))
        .map(|(_conn, r)| r)
}

/// Reads until EOF then writes `to_write` and shuts down the write side,
/// returning the bytes read.
fn read_then_write(
    conn: impl AsyncRead + AsyncWrite,
    read_prefix_len: usize,
    to_write: &'static [u8],
) -> impl Future<Item = Vec<u8>, Error = io::Error> {
    io::read_exact(conn, vec![0; read_prefix_len])
        .and_then(move |(conn, r)| write_and_shutdown(conn, to_write).map(|_conn| r))
}

/// writes `to_write` to `conn` and then shuts down the write side of `conn`.
fn write_and_shutdown<T: AsyncRead + AsyncWrite>(
    conn: T,
    to_write: &'static [u8],
) -> impl Future<Item = T, Error = io::Error> {
    io::write_all(conn, to_write).and_then(|(mut conn, _)| {
        conn.shutdown()?;
        Ok(conn)
    })
}

const PING: &[u8] = b"ping";
const PONG: &[u8] = b"pong";
const START_OF_TLS: &[u8] = &[22, 3, 1]; // ContentType::handshake version 3.1

enum Server<A: Accept<ServerConnection>>
where
    AcceptTls<A, CrtKey>: Accept<<Listen as CoreListen>::Connection>,
{
    Init {
        listen: Listen,
        accept: AcceptTls<A, CrtKey>,
    },
    Accepting(<AcceptTls<A, CrtKey> as Accept<<Listen as CoreListen>::Connection>>::Future),
    Serving(<AcceptTls<A, CrtKey> as Accept<<Listen as CoreListen>::Connection>>::ConnectionFuture),
}

#[derive(Clone)]
struct Target(SocketAddr, Conditional<Name>);

#[derive(Clone)]
struct ClientTls(CrtKey);

impl<A: Accept<ServerConnection> + Clone> Future for Server<A> {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            *self = match self {
                Server::Init {
                    ref mut listen,
                    ref mut accept,
                } => {
                    match Accept::poll_ready(accept) {
                        Ok(Async::Ready(())) => {}
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Err(_) => panic!("accept failed"),
                    }
                    let conn = match listen.poll_accept() {
                        Ok(Async::Ready(conn)) => conn,
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Err(_) => panic!("listener failed"),
                    };
                    Server::Accepting(accept.accept(conn))
                }
                Server::Accepting(ref mut fut) => match fut.poll() {
                    Ok(Async::Ready(conn_future)) => Server::Serving(conn_future),
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Err(_) => panic!("accepting failed"),
                },

                Server::Serving(ref mut fut) => match fut.poll() {
                    Ok(ready) => return Ok(ready),
                    Err(_) => panic!("connection failed"),
                },
            }
        }
    }
}

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

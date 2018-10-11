// These are basically integration tests for the `connection` submodule, but
// they cannot be "real" integration tests because `connection` isn't a public
// interface and because `connection` exposes a `#[cfg(test)]`-only API for use
// by these tests.

use std::{
    net::SocketAddr,
    sync::mpsc,
};

use tokio::{
    self,
    io,
    prelude::*,
};

use app::config::Addr;
use Conditional;

use super::{
    connection::{self, Connection},
    tls,
};

#[test]
fn plaintext() {
    let (client_result, server_result) = run_test(
        Conditional::None(tls::ReasonForNoTls::Disabled),
        |conn| write_then_read(conn, PING),
        Conditional::None(tls::ReasonForNoTls::Disabled),
        |conn| read_then_write(conn, PING.len(), PONG));
    assert_eq!(client_result.is_tls(), false);
    assert_eq!(&client_result.result.unwrap()[..], PONG);
    assert_eq!(server_result.is_tls(), false);
    assert_eq!(&server_result.result.unwrap()[..], PING);
}

#[test]
fn proxy_to_proxy_tls_works() {
    let server_tls = tls::config_test_util::FOO_NS1.server();
    let client_tls = tls::config_test_util::BAR_NS1.client(server_tls.server_identity.clone());
    let (client_result, server_result) = run_test(
        Conditional::Some(client_tls), |conn| write_then_read(conn, PING),
        Conditional::Some(server_tls), |conn| read_then_write(conn, PING.len(), PONG));
    assert_eq!(client_result.is_tls(), true);
    assert_eq!(&client_result.result.unwrap()[..], PONG);
    assert_eq!(server_result.is_tls(), true);
    assert_eq!(&server_result.result.unwrap()[..], PING);
}

#[test]
fn proxy_to_proxy_tls_pass_through_when_identity_does_not_match() {
    let server_tls = tls::config_test_util::FOO_NS1.server();

    // Misuse the client's identity instead of the server's identity. Any
    // identity other than `server_tls.server_identity` would work.
    let client_tls = tls::config_test_util::BAR_NS1.client(
        tls::config_test_util::BAR_NS1.to_settings().pod_identity.clone());

    let (client_result, server_result) = run_test(
        Conditional::Some(client_tls), |conn| write_then_read(conn, PING),
        Conditional::Some(server_tls), |conn| read_then_write(conn, START_OF_TLS.len(), PONG));

    // The server's connection will succeed with the TLS client hello passed
    // through, because the SNI doesn't match its identity.
    assert_eq!(client_result.is_tls(), false);
    assert!(client_result.result.is_err());
    assert_eq!(server_result.is_tls(), false);
    assert_eq!(&server_result.result.unwrap()[..], START_OF_TLS);
}

struct Transported<R> {
    /// The value of `Connection::tls_status()` for the established connection.
    ///
    /// This will be `None` if we never even get a `Connection`.
    tls_status: Option<tls::Status>,

    /// The connection's result.
    result: Result<R, io::Error>,
}

impl<R> Transported<R> {
    fn is_tls(&self) -> bool {
        match &self.tls_status {
            Some(Conditional::Some(())) => true,
            _ => false,
        }
    }
}

/// Runs a test for a single TCP connection. `client` processes the connection
/// on the client side and `server` processes the connection on the server
/// side.
fn run_test<C, CF, CR, S, SF, SR>(
    client_tls: tls::ConditionalConnectionConfig<tls::ClientConfigWatch>,
    client: C,
    server_tls: tls::ConditionalConnectionConfig<tls::ServerConfigWatch>,
    server: S)
    -> (Transported<CR>, Transported<SR>)
    where
        // Client
        C: FnOnce(Connection) -> CF + Send + 'static,
        CF: Future<Item=CR, Error=io::Error> + Send + 'static,
        CR: Send + 'static,
        // Server
        S: Fn(Connection) -> SF + Send + 'static,
        SF: Future<Item=SR, Error=io::Error> + Send + 'static,
        SR: Send + 'static,
{
    let _ = ::env_logger::try_init();

    // A future that will receive a single connection.
    let (server, server_addr, server_result) = {
        // Saves the result of every connection.
        let (sender, receiver) = mpsc::channel::<Transported<SR>>();

        // Let the OS decide the port number and then return the resulting
        // `SocketAddr` so the client can connect to it. This allows multiple
        // tests to run at once, which wouldn't work if they all were bound on
        // a fixed port.
        let addr = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
        let server_bound = connection::BoundPort::new(Addr::from(addr), server_tls)
            .unwrap();
        let server_addr = server_bound.local_addr();

        let connection_limit = 1; // TODO: allow caller to set this.

        let server = server_bound
            .listen_and_fold_n(connection_limit, sender, move |sender, (conn, _)| {
                let tls_status = Some(conn.tls_status());
                trace!("server tls_status: {:?}", tls_status);
                server(conn)
                    .then(move |result| {
                        sender.send(Transported { tls_status, result, }).unwrap();
                        Ok(sender)
                    })
            })
            .map_err(|e| panic!("Unexpected server error: {:?}", e));

        (server, server_addr, receiver)
    };

    // A future that will open a single connection to the server.
    let (client, client_result) = {
        let tls = client_tls.and_then(|conn_cfg| {
            let server_identity = conn_cfg.server_identity.clone();
            (*conn_cfg.config.borrow()).as_ref().map(|cfg| {
                tls::ConnectionConfig {
                    server_identity,
                    config: cfg.clone(),
                }
            })
        });

        // Saves the result of the single connection. This could be a simpler
        // type, e.g. `Arc<Mutex>`, but using a channel simplifies the code and
        // parallels the server side.
        let (sender, receiver) = mpsc::channel::<Transported<CR>>();
        let sender_clone = sender.clone();

        let client = connection::connect(&server_addr, tls)
            .map_err(move |e| {
                sender_clone.send(Transported { tls_status: None, result: Err(e) }).unwrap();
                ()
            })
            .and_then(|conn| {
                let tls_status = Some(conn.tls_status());
                trace!("client tls_status: {:?}", tls_status);
                client(conn)
                    .then(move |result| {
                        sender.send(Transported { tls_status, result }).unwrap();
                        Ok(())
                    })
            });

        (client, receiver)
    };

    tokio::run({
        server.join(client)
            .map(|_| ())
    });

    let client_result = client_result.try_recv().unwrap();

    // XXX: This assumes that only one connection is accepted. TODO: allow the
    // caller to observe the results for every connection, once we have tests
    // that allow accepting multiple connections.
    let server_result = server_result.try_recv().unwrap();

    (client_result, server_result)
}

/// Writes `to_write` and shuts down the write side, then reads until EOF,
/// returning the bytes read.
fn write_then_read(conn: Connection, to_write: &'static [u8])
    -> impl Future<Item=Vec<u8>, Error=io::Error>
{
    write_and_shutdown(conn, to_write)
        .and_then(|conn| io::read_to_end(conn, Vec::new()))
        .map(|(_conn, r)| r)
}

/// Reads until EOF then writes `to_write` and shuts down the write side,
/// returning the bytes read.
fn read_then_write(conn: Connection, read_prefix_len: usize, to_write: &'static [u8])
    -> impl Future<Item=Vec<u8>, Error=io::Error>
{
    io::read_exact(conn, vec![0; read_prefix_len])
        .and_then(move |(conn, r)| {
            write_and_shutdown(conn, to_write)
                .map(|_conn| r)
        })
}


/// writes `to_write` to `conn` and then shuts down the write side of `conn`.
fn write_and_shutdown(conn: connection::Connection, to_write: &'static [u8])
    -> impl Future<Item=Connection, Error=io::Error>
{
    io::write_all(conn, to_write)
        .and_then(|(mut conn, _)| {
            conn.shutdown()?;
            Ok(conn)
        })
}

const PING: &[u8] = b"ping";
const PONG: &[u8] = b"pong";
const START_OF_TLS: &[u8] = &[22, 3, 1]; // ContentType::handshake version 3.1

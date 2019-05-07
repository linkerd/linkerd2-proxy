use support::*;

use std::io;
use std::sync::Mutex;

use self::futures::sync::{mpsc, oneshot};
use self::tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use self::tokio_rustls::TlsStream;
use self::webpki::{DNSName, DNSNameRef};
use rustls::{ClientConfig, ClientSession};
use std::io::{Read, Write};
use std::sync::Arc;
use support::bytes::IntoBuf;
use support::hyper::body::Payload;

type ClientError = hyper::Error;
type Request = http::Request<Bytes>;
type Response = http::Response<BytesBody>;
type Sender = mpsc::UnboundedSender<(Request, oneshot::Sender<Result<Response, ClientError>>)>;

#[derive(Clone)]
pub struct TlsConfig {
    client_config: Arc<ClientConfig>,
    name: DNSName,
}

impl TlsConfig {
    pub fn new(client_config: Arc<ClientConfig>, name: &str) -> Self {
        let dns_name = DNSNameRef::try_from_ascii_str(name)
            .expect("no_fail")
            .to_owned();
        TlsConfig {
            client_config,
            name: dns_name,
        }
    }
}

#[derive(Debug)]
pub struct BytesBody(hyper::Body);

pub fn new<T: Into<String>>(addr: SocketAddr, auth: T) -> Client {
    http2(addr, auth.into())
}

pub fn http1<T: Into<String>>(addr: SocketAddr, auth: T) -> Client {
    Client::new(
        addr,
        auth.into(),
        Run::Http1 {
            absolute_uris: false,
        },
        None,
    )
}

pub fn http1_tls<T: Into<String>>(addr: SocketAddr, auth: T, tls: TlsConfig) -> Client {
    Client::new(
        addr,
        auth.into(),
        Run::Http1 {
            absolute_uris: false,
        },
        Some(tls),
    )
}

/// This sends `GET http://foo.com/ HTTP/1.1` instead of just `GET / HTTP/1.1`.
pub fn http1_absolute_uris<T: Into<String>>(addr: SocketAddr, auth: T) -> Client {
    Client::new(
        addr,
        auth.into(),
        Run::Http1 {
            absolute_uris: true,
        },
        None,
    )
}

pub fn http2<T: Into<String>>(addr: SocketAddr, auth: T) -> Client {
    Client::new(addr, auth.into(), Run::Http2, None)
}

pub fn http2_tls<T: Into<String>>(addr: SocketAddr, auth: T, tls: TlsConfig) -> Client {
    Client::new(addr, auth.into(), Run::Http2, Some(tls))
}

pub fn tcp(addr: SocketAddr) -> tcp::TcpClient {
    tcp::client(addr)
}

pub struct Client {
    authority: String,
    /// This is a future that completes when the associated connection for
    /// this Client has been dropped.
    running: Running,
    tx: Sender,
    version: http::Version,
    tls: Option<TlsConfig>,
}

impl Client {
    fn new(addr: SocketAddr, authority: String, r: Run, tls: Option<TlsConfig>) -> Client {
        let v = match r {
            Run::Http1 { .. } => http::Version::HTTP_11,
            Run::Http2 => http::Version::HTTP_2,
        };
        let (tx, running) = run(addr, r, tls.clone());
        Client {
            authority,
            running,
            tx,
            version: v,
            tls: tls,
        }
    }

    pub fn get(&self, path: &str) -> String {
        let mut req = self.request_builder(path);
        let res = self.request(req.method("GET"));
        assert!(
            res.status().is_success(),
            "client.get({:?}) expects 2xx, got \"{}\"",
            path,
            res.status(),
        );
        let stream = res.into_parts().1;
        stream
            .concat2()
            .map(|body| ::std::str::from_utf8(&body).unwrap().to_string())
            .wait()
            .expect("get() wait body")
    }

    pub fn request_async(
        &self,
        builder: &mut http::request::Builder,
    ) -> Box<Future<Item = Response, Error = ClientError> + Send> {
        self.send_req(builder.body(Bytes::new()).unwrap())
    }

    pub fn request(&self, builder: &mut http::request::Builder) -> Response {
        self.request_async(builder).wait().expect("response")
    }

    pub fn request_body(&self, req: Request) -> Response {
        self.send_req(req).wait().expect("response")
    }

    pub fn request_body_async(
        &self,
        req: Request,
    ) -> Box<Future<Item = Response, Error = ClientError> + Send> {
        self.send_req(req)
    }

    pub fn request_builder(&self, path: &str) -> http::request::Builder {
        let mut b = ::http::Request::builder();

        if self.tls.is_some() {
            b.uri(format!("https://{}{}", self.authority, path).as_str())
                .version(self.version);
        } else {
            b.uri(format!("http://{}{}", self.authority, path).as_str())
                .version(self.version);
        };

        b
    }

    fn send_req(
        &self,
        mut req: Request,
    ) -> Box<Future<Item = Response, Error = ClientError> + Send> {
        if req.uri().scheme_part().is_none() {
            if self.tls.is_some() {
                *req.uri_mut() = format!("https://{}{}", self.authority, req.uri().path())
                    .parse()
                    .unwrap();
            } else {
                *req.uri_mut() = format!("http://{}{}", self.authority, req.uri().path())
                    .parse()
                    .unwrap();
            };
        }
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.unbounded_send((req, tx));
        Box::new(rx.then(|oneshot_result| oneshot_result.expect("request canceled")))
    }

    pub fn wait_for_closed(self) {
        self.running.wait().expect("wait_for_closed");
    }
}

#[derive(Debug)]
enum Run {
    Http1 { absolute_uris: bool },
    Http2,
}

fn run(addr: SocketAddr, version: Run, tls: Option<TlsConfig>) -> (Sender, Running) {
    let (tx, rx) = mpsc::unbounded::<(Request, oneshot::Sender<Result<Response, ClientError>>)>();
    let (running_tx, running_rx) = running();

    let tname = format!("support {:?} server (test={})", version, thread_name(),);

    ::std::thread::Builder::new()
        .name(tname)
        .spawn(move || {
            let mut runtime =
                runtime::current_thread::Runtime::new().expect("initialize support client runtime");

            let absolute_uris = if let Run::Http1 { absolute_uris } = version {
                absolute_uris
            } else {
                false
            };
            let conn = Conn {
                addr,
                running: Arc::new(Mutex::new(Some(running_tx))),
                absolute_uris,
                tls,
            };

            let http2_only = match version {
                Run::Http1 { .. } => false,
                Run::Http2 => true,
            };

            let client = hyper::Client::builder()
                .http2_only(http2_only)
                .build::<Conn, hyper::Body>(conn);

            let work = rx
                .for_each(move |(req, cb)| {
                    let req = req.map(hyper::Body::from);
                    let fut = client.request(req).then(move |result| {
                        let result = result.map(|resp| resp.map(BytesBody));
                        let _ = cb.send(result);
                        Ok(())
                    });
                    tokio::spawn(fut);
                    Ok(())
                })
                .map_err(|e| println!("client error: {:?}", e));

            runtime.block_on(work).expect("support client runtime");
        })
        .expect("thread spawn");
    (tx, running_rx)
}

struct Conn {
    addr: SocketAddr,
    running: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    absolute_uris: bool,
    tls: Option<TlsConfig>,
}

impl Conn {
    fn connect_(&self) -> Box<Future<Item = RunningIo, Error = ::std::io::Error> + Send> {
        Box::new(ConnectorFuture::Init {
            future: TcpStream::connect(&self.addr),
            tls: self.tls.clone(),
            running: self.running.clone(),
        })
    }
}

impl Connect for Conn {
    type Connected = RunningIo;
    type Error = ::std::io::Error;
    type Future = Box<Future<Item = Self::Connected, Error = ::std::io::Error>>;

    fn connect(&self) -> Self::Future {
        self.connect_()
    }
}

impl hyper::client::connect::Connect for Conn {
    type Transport = RunningIo;
    type Future = Box<
        Future<
                Item = (Self::Transport, hyper::client::connect::Connected),
                Error = ::std::io::Error,
            > + Send,
    >;
    type Error = ::std::io::Error;
    fn connect(&self, _: hyper::client::connect::Destination) -> Self::Future {
        let connected = hyper::client::connect::Connected::new().proxy(self.absolute_uris);
        Box::new(self.connect_().map(|t| (t, connected)))
    }
}

enum ConnectorFuture {
    Init {
        future: tokio::net::tcp::ConnectFuture,
        tls: Option<TlsConfig>,
        running: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    },
    Handshake {
        future: tokio_rustls::Connect<TcpStream>,
        running: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    },
}

impl Future for ConnectorFuture {
    type Item = RunningIo;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let take_running = move |r: &Mutex<Option<oneshot::Sender<()>>>| {
            r.lock()
                .expect("running lock")
                .take()
                .expect("support client cannot connect more than once")
        };

        loop {
            *self = match self {
                ConnectorFuture::Init {
                    future,
                    tls,
                    running,
                } => {
                    let io = try_ready!(future.poll());

                    match tls {
                        None => {
                            return Ok(Async::Ready(RunningIo::Http(io, take_running(running))));
                        }

                        Some(TlsConfig {
                            client_config,
                            name,
                        }) => {
                            let future = tokio_rustls::TlsConnector::from(client_config.clone())
                                .connect(DNSName::as_ref(name), io);
                            ConnectorFuture::Handshake {
                                future,
                                running: running.clone(),
                            }
                        }
                    }
                }

                ConnectorFuture::Handshake { future, running } => {
                    let io = try_ready!(future.poll());
                    return Ok(Async::Ready(RunningIo::Https(io, take_running(running))));
                }
            }
        }
    }
}

enum RunningIo {
    Http(TcpStream, oneshot::Sender<()>),
    Https(TlsStream<TcpStream, ClientSession>, oneshot::Sender<()>),
}

impl Read for RunningIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match *self {
            RunningIo::Http(ref mut s, _) => s.read(buf),
            RunningIo::Https(ref mut s, _) => s.read(buf),
        }
    }
}

impl Write for RunningIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match *self {
            RunningIo::Http(ref mut s, _) => s.write(buf),
            RunningIo::Https(ref mut s, _) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match *self {
            RunningIo::Http(ref mut s, _) => s.flush(),
            RunningIo::Https(ref mut s, _) => s.flush(),
        }
    }
}

impl AsyncRead for RunningIo {}

impl AsyncWrite for RunningIo {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        match *self {
            RunningIo::Http(ref mut s, _) => s.shutdown(),
            RunningIo::Https(ref mut s, _) => s.shutdown(),
        }
    }
}

impl HttpBody for BytesBody {
    type Item = <Bytes as IntoBuf>::Buf;
    type Error = hyper::Error;

    fn poll_buf(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match try_ready!(self.0.poll_data()) {
            Some(chunk) => Ok(Async::Ready(Some(Bytes::from(chunk).into_buf()))),
            None => Ok(Async::Ready(None)),
        }
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        self.0.poll_trailers()
    }
}

impl Stream for BytesBody {
    type Item = Bytes;
    type Error = hyper::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match try_ready!(self.0.poll_data()) {
            Some(chunk) => Ok(Async::Ready(Some(chunk.into()))),
            None => Ok(Async::Ready(None)),
        }
    }
}

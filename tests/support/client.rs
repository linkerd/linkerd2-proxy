use support::*;

use std::io;
use std::sync::Mutex;

use self::futures::{
    future::Executor,
    sync::{mpsc, oneshot},
};
use self::tokio::{
    net::TcpStream,
    io::{AsyncRead, AsyncWrite},
};
use support::hyper::body::Payload;

type Request = http::Request<Bytes>;
type Response = http::Response<BytesBody>;
type Sender = mpsc::UnboundedSender<(Request, oneshot::Sender<Result<Response, String>>)>;

#[derive(Debug)]
pub struct BytesBody(hyper::Body);

pub fn new<T: Into<String>>(addr: SocketAddr, auth: T) -> Client {
    http2(addr, auth.into())
}

pub fn http1<T: Into<String>>(addr: SocketAddr, auth: T) -> Client {
    Client::new(addr, auth.into(), Run::Http1 {
        absolute_uris: false,
    })
}

/// This sends `GET http://foo.com/ HTTP/1.1` instead of just `GET / HTTP/1.1`.
pub fn http1_absolute_uris<T: Into<String>>(addr: SocketAddr, auth: T) -> Client {
    Client::new(addr, auth.into(), Run::Http1 {
        absolute_uris: true,
    })
}

pub fn http2<T: Into<String>>(addr: SocketAddr, auth: T) -> Client {
    Client::new(addr, auth.into(), Run::Http2)
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
}

impl Client {
    fn new(addr: SocketAddr, authority: String, r: Run) -> Client {
        let v = match r {
            Run::Http1 { .. } => http::Version::HTTP_11,
            Run::Http2 => http::Version::HTTP_2,
        };
        let (tx, running) = run(addr, r);
        Client {
            authority,
            running,
            tx,
            version: v,
        }
    }

    pub fn get(&self, path: &str) -> String {
        let mut req = self.request_builder(path);
        let res = self.request(req.method("GET"));
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "client.get({:?}) expects 200 OK, got \"{}\"",
            path,
            res.status(),
        );
        let stream = res.into_parts().1;
        stream.concat2()
            .map(|body| ::std::str::from_utf8(&body).unwrap().to_string())
            .wait()
            .expect("get() wait body")
    }

    pub fn request_async(&self, builder: &mut http::request::Builder) -> Box<Future<Item=Response, Error=String> + Send> {
        self.send_req(builder.body(Bytes::new()).unwrap())
    }

    pub fn request(&self, builder: &mut http::request::Builder) -> Response {
        self.request_async(builder)
            .wait()
            .expect("response")
    }

    pub fn request_body(&self, req: Request) -> Response {
        self.send_req(req)
            .wait()
            .expect("response")
    }

    pub fn request_body_async(&self, req: Request) -> Box<Future<Item=Response, Error=String> + Send> {
        self.send_req(req)
    }

    pub fn request_builder(&self, path: &str) -> http::request::Builder {
        let mut b = ::http::Request::builder();
        b.uri(format!("http://{}{}", self.authority, path).as_str())
            .version(self.version);
        b
    }

    fn send_req(&self, mut req: Request) -> Box<Future<Item=Response, Error=String> + Send> {
        if req.uri().scheme_part().is_none() {
            let absolute = format!("http://{}{}", self.authority, req.uri().path()).parse().unwrap();
            *req.uri_mut() = absolute;
        }
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.unbounded_send((req, tx));
        Box::new(rx.then(|oneshot_result| oneshot_result.expect("request canceled")))
    }

    pub fn wait_for_closed(self) {
        self.running
            .wait()
            .expect("wait_for_closed");
    }
}

enum Run {
    Http1 {
        absolute_uris: bool,
    },
    Http2,
}

fn run(addr: SocketAddr, version: Run) -> (Sender, Running) {
    let (tx, rx) = mpsc::unbounded::<(Request, oneshot::Sender<Result<Response, String>>)>();
    let (running_tx, running_rx) = running();

    ::std::thread::Builder::new()
        .name("support client".into())
        .spawn(move || {
        let mut runtime = runtime::current_thread::Runtime::new()
            .expect("initialize support client runtime");

        let absolute_uris = if let Run::Http1 { absolute_uris } = version {
            absolute_uris
        } else {
            false
        };
        let conn = Conn {
            addr,
            running: Mutex::new(Some(running_tx)),
            absolute_uris,
        };

        let http2_only = match version {
            Run::Http1 { .. } => false,
            Run::Http2 => true,
        };

        let client = hyper::Client::builder()
            .http2_only(http2_only)
            .build::<Conn, hyper::Body>(conn);

        let work = rx.for_each(move |(req, cb)| {
            let req = req.map(hyper::Body::from);
            let fut = client.request(req).then(move |result| {
                let result = result
                    .map(|resp| resp.map(BytesBody))
                    .map_err(|e| e.to_string());
                let _ = cb.send(result);
                Ok(())
            });
            current_thread::TaskExecutor::current().execute(fut)
                .map_err(|e| println!("client spawn error: {:?}", e))
        })
            .map_err(|e| println!("client error: {:?}", e));

        runtime.block_on(work).expect("support client runtime");
    }).unwrap();
    (tx, running_rx)
}

/// The "connector". Clones `running` into new connections, so we can signal
/// when all connections are finally closed.
struct Conn {
    addr: SocketAddr,
    /// When this Sender drops, that should mean the connection is closed.
    running: Mutex<Option<oneshot::Sender<()>>>,
    absolute_uris: bool,
}

impl Conn {
    fn connect_(&self) -> Box<Future<Item = RunningIo, Error = ::std::io::Error> + Send> {
        let running = self.running
            .lock()
            .expect("running lock")
            .take()
            .expect("connected more than once");
        let c = TcpStream::connect(&self.addr)
            .and_then(|tcp| tcp.set_nodelay(true).map(move |_| tcp))
            .map(move |tcp| RunningIo {
                inner: tcp,
                running: running,
            });
        Box::new(c)
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
    type Future = Box<Future<
        Item = (Self::Transport, hyper::client::connect::Connected),
        Error = ::std::io::Error,
    > + Send>;
    type Error = ::std::io::Error;
    fn connect(&self, _: hyper::client::connect::Destination) -> Self::Future {
        let connected = hyper::client::connect::Connected::new()
            .proxy(self.absolute_uris);
        Box::new(self.connect_().map(|t| (t, connected)))
    }
}

/// A wrapper around a TcpStream, allowing us to signal when the connection
/// is dropped.
struct RunningIo {
    inner: TcpStream,
    /// When this drops, the related Receiver is notified that the connection
    /// is closed.
    running: oneshot::Sender<()>,
}

impl io::Read for RunningIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl io::Write for RunningIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl AsyncRead for RunningIo {}

impl AsyncWrite for RunningIo {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        AsyncWrite::shutdown(&mut self.inner)
    }
}

impl BytesBody {
    pub fn poll_data(&mut self) -> Poll<Option<Bytes>, hyper::Error> {
        match try_ready!(self.0.poll_data()) {
            Some(chunk) => Ok(Async::Ready(Some(chunk.into()))),
            None => Ok(Async::Ready(None)),
        }
    }

    pub fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, hyper::Error> {
        self.0.poll_trailers()
    }
}

impl Stream for BytesBody {
    type Item = Bytes;
    type Error = hyper::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.poll_data()
    }
}


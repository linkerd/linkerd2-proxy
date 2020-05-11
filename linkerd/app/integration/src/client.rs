use super::*;
use rustls::ClientConfig;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tracing::info_span;
use tracing_futures::Instrument;
use webpki::{DNSName, DNSNameRef};

type ClientError = hyper::Error;
type Request = http::Request<Bytes>;
type Response = http::Response<hyper::Body>;
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
        futures::executor::block_on(self.get_async(path))
    }

    pub async fn get_async(&self, path: &str) -> String {
        let req = self.request_builder(path);
        let res = self
            .request_async(req.method("GET"))
            .await
            .expect("response");
        assert!(
            res.status().is_success(),
            "client.get({:?}) expects 2xx, got \"{}\"",
            path,
            res.status(),
        );
        let stream = res.into_parts().1;
        let mut body = hyper::body::aggregate(stream).await.expect("wait body");
        std::str::from_utf8(body.to_bytes().as_ref())
            .unwrap()
            .to_string()
    }

    pub async fn request_async(
        &self,
        builder: http::request::Builder,
    ) -> Result<Response, ClientError> {
        self.send_req(builder.body(Bytes::new()).unwrap()).await
    }

    pub fn request(&self, builder: http::request::Builder) -> Response {
        futures::executor::block_on(self.request_async(builder)).expect("response")
    }

    #[tokio::main]
    pub async fn request_body(&self, req: Request) -> Response {
        self.send_req(req).await.expect("response")
    }

    pub async fn request_body_async(&self, req: Request) -> Result<Response, ClientError> {
        self.send_req(req).await
    }

    pub fn request_builder(&self, path: &str) -> http::request::Builder {
        let b = ::http::Request::builder();

        if self.tls.is_some() {
            b.uri(format!("https://{}{}", self.authority, path).as_str())
                .version(self.version)
        } else {
            b.uri(format!("http://{}{}", self.authority, path).as_str())
                .version(self.version)
        }
    }

    #[tracing::instrument(skip(self))]
    pub(crate) fn send_req(
        &self,
        mut req: Request,
    ) -> impl Future<Output = Result<Response, ClientError>> + Send + Sync + 'static {
        if req.uri().scheme().is_none() {
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
        tracing::debug!(headers = ?req.headers(), "request");
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send((req, tx));
        async { rx.await.expect("request cancelled") }.in_current_span()
    }

    pub fn wait_for_closed(self) {
        futures::executor::block_on(self.running)
    }
}

#[derive(Debug)]
enum Run {
    Http1 { absolute_uris: bool },
    Http2,
}

fn run(addr: SocketAddr, version: Run, tls: Option<TlsConfig>) -> (Sender, Running) {
    let (tx, rx) =
        mpsc::unbounded_channel::<(Request, oneshot::Sender<Result<Response, ClientError>>)>();
    let (running_tx, running_rx) = running();

    let tname = format!("support {:?} server (test={})", version, thread_name(),);

    ::std::thread::Builder::new()
        .name(tname)
        .spawn(move || {
            let mut runtime = tokio::runtime::Builder::new()
                .enable_all()
                .basic_scheduler()
                .build()
                .expect("initialize support client runtime");

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

            let span = info_span!("test client", peer_addr = %addr);
            let client = hyper::Client::builder()
                .http2_only(http2_only)
                .build::<Conn, hyper::Body>(conn);

            let work = async move {
                let mut rx = rx;
                while let Some((req, cb)) = rx.recv().await {
                    let req = req.map(hyper::Body::from);
                    let req = client.request(req);
                    tokio::spawn(
                        async move {
                            let result = req.await;
                            let _ = cb.send(result);
                        }
                        .in_current_span(),
                    );
                }
            };

            runtime.block_on(work.instrument(span));
        })
        .expect("thread spawn");
    (tx, running_rx)
}

#[derive(Clone)]
struct Conn {
    addr: SocketAddr,
    running: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    absolute_uris: bool,
    tls: Option<TlsConfig>,
}

impl tower::Service<hyper::Uri> for Conn {
    type Response = RunningIo;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;
    type Error = io::Error;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: hyper::Uri) -> Self::Future {
        let running = self.running.clone();
        let tls = self.tls.clone();
        let conn = TcpStream::connect(self.addr.clone());
        let abs_form = self.absolute_uris;
        Box::pin(async move {
            let io = conn.await?;

            let io = if let Some(TlsConfig {
                name,
                client_config,
            }) = tls
            {
                let io = tokio_rustls::TlsConnector::from(client_config.clone())
                    .connect(DNSName::as_ref(&name), io)
                    .await?;
                Box::pin(io) as Pin<Box<dyn Io + Send + 'static>>
            } else {
                Box::pin(io) as Pin<Box<dyn Io + Send + 'static>>
            };

            let running = running
                .lock()
                .expect("running lock")
                .take()
                .expect("support client cannot connect more than once");
            Ok(RunningIo {
                io,
                abs_form,
                _running: Some(running),
            })
        })
    }
}

impl hyper::client::connect::Connection for RunningIo {
    fn connected(&self) -> hyper::client::connect::Connected {
        hyper::client::connect::Connected::new().proxy(self.abs_form)
    }
}

use super::*;
use futures::TryFuture;
use http::Response;
use linkerd2_app_core::proxy::http::trace;
use rustls::ServerConfig;
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::thread;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio_rustls::TlsAcceptor;
use tracing_futures::Instrument;

pub fn new() -> Server {
    http2()
}

pub fn http1() -> Server {
    Server::http1()
}

pub fn http1_tls(tls: Arc<ServerConfig>) -> Server {
    Server::http1_tls(tls)
}

pub fn http2() -> Server {
    Server::http2()
}

pub fn http2_tls(tls: Arc<ServerConfig>) -> Server {
    Server::http2_tls(tls)
}

pub fn tcp() -> tcp::TcpServer {
    tcp::server()
}

pub struct Server {
    routes: HashMap<String, Route>,
    version: Run,
    tls: Option<Arc<ServerConfig>>,
}

pub struct Listening {
    pub addr: SocketAddr,
    pub(super) _shutdown: Shutdown,
    pub(super) conn_count: Arc<AtomicUsize>,
}

pub fn mock_listening(a: SocketAddr) -> Listening {
    let (tx, _rx) = shutdown_signal();
    let conn_count = Arc::new(AtomicUsize::from(0));
    Listening {
        addr: a,
        _shutdown: tx,
        conn_count,
    }
}

impl Listening {
    pub fn connections(&self) -> usize {
        self.conn_count.load(Ordering::Acquire)
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        println!("server Listening dropped; addr={}", self.addr);
    }
}

impl Server {
    fn new(run: Run, tls: Option<Arc<ServerConfig>>) -> Self {
        Server {
            routes: HashMap::new(),
            version: run,
            tls,
        }
    }
    fn http1() -> Self {
        Server::new(Run::Http1, None)
    }

    fn http1_tls(tls: Arc<ServerConfig>) -> Self {
        Server::new(Run::Http1, Some(tls))
    }

    fn http2() -> Self {
        Server::new(Run::Http2, None)
    }

    fn http2_tls(tls: Arc<ServerConfig>) -> Self {
        Server::new(Run::Http2, Some(tls))
    }

    /// Return a string body as a 200 OK response, with the string as
    /// the response body.
    pub fn route(mut self, path: &str, resp: &str) -> Self {
        self.routes.insert(path.into(), Route::string(resp));
        self
    }

    /// Call a closure when the request matches, returning a response
    /// to send back.
    pub fn route_fn<F>(self, path: &str, cb: F) -> Self
    where
        F: Fn(Request<ReqBody>) -> Response<Bytes> + Send + 'static,
    {
        self.route_async(path, move |req| {
            let res = cb(req);
            async move { Ok::<_, BoxError>(res) }
        })
    }

    /// Call a closure when the request matches, returning a Future of
    /// a response to send back.
    pub fn route_async<F, U>(mut self, path: &str, cb: F) -> Self
    where
        F: Fn(Request<ReqBody>) -> U + Send + 'static,
        U: TryFuture<Ok = Response<Bytes>> + Send + Sync + 'static,
        U::Error: Into<BoxError> + Send + 'static,
    {
        let func = move |req| {
            Box::pin(cb(req).map_err(Into::into))
                as Pin<
                    Box<
                        dyn Future<Output = Result<Response<Bytes>, BoxError>>
                            + Send
                            + Sync
                            + 'static,
                    >,
                >
        };
        self.routes.insert(path.into(), Route(Box::new(func)));
        self
    }

    pub fn route_with_latency(self, path: &str, resp: &str, latency: Duration) -> Self {
        let resp = Bytes::from(resp.to_string());
        self.route_fn(path, move |_| {
            thread::sleep(latency);
            http::Response::builder()
                .status(200)
                .body(resp.clone())
                .unwrap()
        })
    }

    pub fn delay_listen<F>(self, f: F) -> Listening
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.run_inner(Some(Box::pin(f)))
    }

    pub fn run(self) -> Listening {
        self.run_inner(None)
    }

    fn run_inner(
        self,
        delay: Option<Pin<Box<dyn Future<Output = ()> + Send + 'static>>>,
    ) -> Listening {
        let (tx, rx) = shutdown_signal();
        let (listening_tx, listening_rx) = oneshot::channel();
        let mut listening_tx = Some(listening_tx);
        let conn_count = Arc::new(AtomicUsize::from(0));
        let srv_conn_count = Arc::clone(&conn_count);
        let version = self.version;
        let tname = format!("support {:?} server (test={})", version, thread_name(),);

        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let listener = net2::TcpBuilder::new_v4().expect("Tcp::new_v4");
        listener.bind(addr).expect("Tcp::bind");
        let addr = listener.local_addr().expect("Tcp::local_addr");

        let tls_config = self.tls.clone();
        ::std::thread::Builder::new()
            .name(tname)
            .spawn(move || {
                let (subscriber, _) = trace_init();
                let _subscriber = subscriber.set_default();
                tracing::info!("support server running");
                let mut new_svc = NewSvc(Arc::new(self.routes));
                let mut http =
                    hyper::server::conn::Http::new().with_executor(trace::Executor::new());
                match self.version {
                    Run::Http1 => http.http1_only(true),
                    Run::Http2 => http.http2_only(true),
                };
                let serve = async move {
                    if let Some(delay) = delay {
                        let _ = listening_tx.take().unwrap().send(());
                        delay.await;
                    }
                    let listener = listener.listen(1024).expect("Tcp::listen");
                    let mut listener = TcpListener::from_std(listener).expect("from_std");

                    if let Some(listening_tx) = listening_tx {
                        let _ = listening_tx.send(());
                    }
                    tracing::info!("listening!");
                    loop {
                        let (sock, addr) = listener.accept().await?;
                        let span = tracing::debug_span!("conn", %addr);
                        let sock = accept_connection(sock, tls_config.clone())
                            .instrument(span.clone())
                            .await?;
                        let http = http.clone();
                        let srv_conn_count = srv_conn_count.clone();
                        let svc = new_svc.call(());
                        tokio::task::spawn_local(
                            async move {
                                tracing::trace!("serving...");
                                let svc = svc.await;
                                tracing::trace!("service acquired");
                                srv_conn_count.fetch_add(1, Ordering::Release);
                                let svc = svc.map_err(|e| {
                                    println!("support/server new_service error: {}", e)
                                })?;
                                let result = http
                                    .serve_connection(sock, svc)
                                    .await
                                    .map_err(|e| println!("support/server error: {}", e));
                                tracing::trace!(?result, "serve done");
                                result
                            }
                            .instrument(span.clone()),
                        );
                    }
                };

                let mut rt = tokio::runtime::Builder::new()
                    .basic_scheduler()
                    .enable_all()
                    .build()
                    .expect("initialize support server runtime");
                tokio::task::LocalSet::new().block_on(
                    &mut rt,
                    async move {
                        tokio::select! {
                            res = serve => res,
                            _ = rx => { Ok::<(), io::Error>(())},
                        }
                    }
                    .instrument(tracing::info_span!("test_server", ?version, %addr)),
                )
            })
            .unwrap();

        futures::executor::block_on(listening_rx).expect("listening_rx");

        // printlns will show if the test fails...
        println!("{:?} server running; addr={}", version, addr,);

        Listening {
            addr,
            _shutdown: tx,
            conn_count,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Run {
    Http1,
    Http2,
}

struct Route(
    Box<
        dyn Fn(
                Request<ReqBody>,
            ) -> Pin<
                Box<
                    dyn Future<Output = Result<http::Response<Bytes>, BoxError>>
                        + Send
                        + Sync
                        + 'static,
                >,
            > + Send,
    >,
);

impl Route {
    fn string(body: &str) -> Route {
        let body = Bytes::from(body.to_string());
        Route(Box::new(move |_| {
            Box::pin(future::ok(
                http::Response::builder()
                    .status(200)
                    .body(body.clone())
                    .unwrap(),
            ))
        }))
    }
}

impl std::fmt::Debug for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Route")
    }
}

type ReqBody = Pin<Box<dyn Stream<Item = Bytes> + Send + Sync>>;
type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug)]
struct Svc(Arc<HashMap<String, Route>>);

impl Svc {
    fn route(
        &mut self,
        req: Request<ReqBody>,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Bytes>, BoxError>> + Send + Sync + 'static>>
    {
        match self.0.get(req.uri().path()) {
            Some(Route(ref func)) => {
                tracing::trace!(path = %req.uri().path(), "found route for path");
                func(req)
            }
            None => {
                println!("server 404: {:?}", req.uri().path());
                let res = http::Response::builder()
                    .status(404)
                    .body(Default::default())
                    .unwrap();
                Box::pin(async move { Ok(res) })
            }
        }
    }
}

impl tower::Service<Request<hyper::Body>> for Svc {
    type Response = Response<hyper::Body>;
    type Error = BoxError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Response<hyper::Body>, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: hyper::Request<hyper::Body>) -> Self::Future {
        let req =
            req.map(|body| Box::pin(body.map(|chunk| chunk.expect("body error!"))) as ReqBody);
        Box::pin(
            self.route(req)
                .map_ok(|res| res.map(|s| hyper::Body::from(s))),
        )
    }
}

#[derive(Debug)]
struct NewSvc(Arc<HashMap<String, Route>>);

impl Service<()> for NewSvc {
    type Response = Svc;
    type Error = ::std::io::Error;
    type Future = future::Ready<Result<Svc, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: ()) -> Self::Future {
        future::ok(Svc(Arc::clone(&self.0)))
    }
}

async fn accept_connection(
    io: TcpStream,
    tls: Option<Arc<ServerConfig>>,
) -> Result<RunningIo, io::Error> {
    tracing::debug!(tls = tls.is_some(), "accepting connection");
    let res = match tls {
        Some(cfg) => {
            let io = TlsAcceptor::from(cfg).accept(io).await?;
            Ok(RunningIo {
                io: Box::pin(io),
                abs_form: false,
                _running: None,
            })
        }

        None => Ok(RunningIo {
            io: Box::pin(io),
            abs_form: false,
            _running: None,
        }),
    };
    tracing::trace!("connection accepted");
    res
}

use super::*;
use futures::TryFutureExt;
use std::collections::VecDeque;
use std::io;
use std::net::TcpListener as StdTcpListener;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tracing_futures::Instrument;

type TcpSender = mpsc::UnboundedSender<oneshot::Sender<TcpConnSender>>;
type TcpConnSender = mpsc::UnboundedSender<(
    Option<Vec<u8>>,
    oneshot::Sender<io::Result<Option<Vec<u8>>>>,
)>;

pub fn client(addr: SocketAddr) -> TcpClient {
    let tx = run_client(addr);
    TcpClient { addr, tx }
}

pub fn server() -> TcpServer {
    TcpServer {
        accepts: VecDeque::new(),
    }
}

pub struct TcpClient {
    addr: SocketAddr,
    tx: TcpSender,
}

type Handler = Box<dyn CallBox + Send>;

trait CallBox: 'static {
    fn call_box(
        self: Box<Self>,
        sock: TcpStream,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
}

impl<F> CallBox for F
where
    F: FnOnce(TcpStream) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> + 'static,
{
    fn call_box(
        self: Box<Self>,
        sock: TcpStream,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        (*self)(sock)
    }
}

pub struct TcpServer {
    accepts: VecDeque<Handler>,
}

pub struct TcpConn {
    addr: SocketAddr,
    tx: TcpConnSender,
}

impl TcpClient {
    pub fn connect(&self) -> TcpConn {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(tx);
        let tx = futures::executor::block_on(rx).unwrap();
        println!("tcp client (addr={}): connected", self.addr);
        TcpConn {
            addr: self.addr,
            tx,
        }
    }
}

impl TcpServer {
    pub fn accept<F, U>(self, cb: F) -> Self
    where
        F: FnOnce(Vec<u8>) -> U + Send + 'static,
        U: Into<Vec<u8>>,
    {
        self.accept_fut(move |mut sock| {
            Box::pin(
                async move {
                    let mut vec = vec![0; 1024];
                    let n = sock.read(&mut vec).await?;
                    vec.truncate(n);
                    let write = cb(vec).into();
                    sock.write_all(&write).await
                }
                .map(|r| match r {
                    Err(e) => panic!("tcp server error: {}", e),
                    _ => {}
                }),
            )
        })
    }

    pub fn accept_fut<F, U>(mut self, cb: F) -> Self
    where
        F: FnOnce(TcpStream) -> U + Send + 'static,
        U: Future<Output = ()> + Send + 'static,
    {
        self.accepts.push_back(Box::new(
            move |tcp| -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> { Box::pin(cb(tcp)) },
        ));
        self
    }

    pub async fn run(self) -> server::Listening {
        run_server(self).await
    }
}

impl TcpConn {
    pub fn read(&self) -> Vec<u8> {
        self.try_read()
            .unwrap_or_else(|e| panic!("TcpConn(addr={}) read() error: {:?}", self.addr, e))
    }

    #[tokio::main(basic_scheduler)]
    pub async fn read_timeout(&self, timeout: Duration) -> Vec<u8> {
        tokio::time::timeout(timeout, self.read_future())
            .await
            .unwrap_or_else(|e| panic!("TcpConn(addr={}) read() error: {:?}", self.addr, e))
            .unwrap_or_else(|e| panic!("TcpConn(addr={}) read() error: {:?}", self.addr, e))
    }

    async fn read_future(&self) -> Result<Vec<u8>, io::Error> {
        println!("tcp client (addr={}): read", self.addr);
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send((None, tx));
        let res = rx.await.expect("tcp read dropped");
        res.map(|opt| opt.unwrap())
    }

    pub fn try_read(&self) -> io::Result<Vec<u8>> {
        tokio::runtime::Builder::new()
            .enable_all()
            .basic_scheduler()
            .build()
            .unwrap()
            .block_on(self.read_future())
    }

    pub fn write<T: Into<Vec<u8>>>(&self, buf: T) {
        println!("tcp client (addr={}): write", self.addr);
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send((Some(buf.into()), tx));
        futures::executor::block_on(rx.map_ok(|rsp| assert!(rsp.unwrap().is_none())))
            .expect("tcp write dropped")
    }
}

fn run_client(addr: SocketAddr) -> TcpSender {
    let (tx, rx) = mpsc::unbounded_channel::<
        oneshot::Sender<
            mpsc::UnboundedSender<(
                Option<Vec<u8>>,
                oneshot::Sender<Result<Option<Vec<u8>>, io::Error>>,
            )>,
        >,
    >();
    let tname = format!("support tcp client (addr={})", addr);
    ::std::thread::Builder::new()
        .name(tname)
        .spawn(move || {
            let mut core = tokio_compat::runtime::current_thread::Runtime::new()
                .expect("support tcp client runtime");
            let mut rx = rx;
            let work = async move {
                while let Some(cb) = rx.recv().await {
                    let tcp = TcpStream::connect(&addr).await.expect("tcp connect error");
                    let (tx, rx) = mpsc::unbounded_channel::<(
                        Option<Vec<u8>>,
                        oneshot::Sender<Result<Option<Vec<u8>>, io::Error>>,
                    )>();
                    cb.send(tx).unwrap();
                    tokio::spawn(async move {
                        let mut rx = rx;
                        let mut tcp = tcp;
                        while let Some((action, cb)) = rx.recv().await {
                            match action {
                                None => {
                                    let mut vec = vec![0; 1024];
                                    match tcp.read(&mut vec).await {
                                        Ok(n) => {
                                            vec.truncate(n);
                                            let _ = cb.send(Ok(Some(vec)));
                                        }
                                        Err(e) => {
                                            cb.send(Err(e)).unwrap();
                                            break;
                                        }
                                    }
                                }
                                Some(vec) => match tcp.write_all(&vec).await {
                                    Ok(_) => {
                                        let _ = cb.send(Ok(None));
                                    }
                                    Err(e) => {
                                        cb.send(Err(e)).unwrap();
                                        break;
                                    }
                                },
                            }
                        }
                    });
                }
            };
            core.block_on_std(work);
        })
        .unwrap();

    println!("tcp client (addr={}) thread running", addr);
    tx
}

async fn run_server(tcp: TcpServer) -> server::Listening {
    let (tx, rx) = shutdown_signal();
    let (started_tx, started_rx) = oneshot::channel();
    let conn_count = Arc::new(AtomicUsize::from(0));
    let srv_conn_count = Arc::clone(&conn_count);
    let any_port = SocketAddr::from(([127, 0, 0, 1], 0));
    let std_listener = StdTcpListener::bind(&any_port).expect("bind");
    let addr = std_listener.local_addr().expect("local_addr");
    let jh = tokio::spawn(
        async move {
            let mut accepts = tcp.accepts;
            let (drain_tx, drain) = drain::channel();
            let listen = async move {
                let mut listener =
                    TcpListener::from_std(std_listener).expect("TcpListener::from_std");

                loop {
                    let (sock, _) = listener.accept().await.unwrap();
                    let cb = accepts.pop_front().expect("no more accepts");
                    srv_conn_count.fetch_add(1, Ordering::Release);

                    let fut = cb.call_box(sock);
                    tokio::spawn(cancelable(drain.clone(), async move {
                        fut.await;
                        Ok::<(), ()>(())
                    }));
                }
            };

            let _ = started_tx.send(());
            tokio::select! {
                _ = rx => {
                    tracing::trace!("shutting down...");
                    drain_tx.drain().await;
                    tracing::trace!("shut down");
                },
                _ = listen => { },
            }
            Ok(())
        }
        .instrument(tracing::info_span!("tcp_server", %addr)),
    );

    started_rx.await.expect("support tcp server started");

    // printlns will show if the test fails...
    println!("tcp server (addr={}): running", addr);

    server::Listening {
        addr,
        shutdown: Some(tx),
        conn_count,
        jh,
    }
}

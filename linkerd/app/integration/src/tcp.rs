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
    fn call_box(self: Box<Self>, sock: TcpStream) -> Pin<Box<dyn Future<Output = ()>>>;
}

impl<F> CallBox for F
where
    F: FnOnce(TcpStream) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
{
    fn call_box(self: Box<Self>, sock: TcpStream) -> Pin<Box<dyn Future<Output = ()>>> {
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
        let _ = self.tx.unbounded_send(tx);
        let tx = rx
            .map_err(|_| panic!("tcp connect dropped"))
            .wait()
            .unwrap();
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
        self.accept_fut(move |sock| {
            async move {
                let mut vec = vec![0; 1024];
                let n = sock.read(&mut vec).await?;
                vec.truncate(n);
                let write = cb(vec).into();
                sock.write_all(vec).await?
            }
            .map_err(|e| panic!("tcp server error: {}", e))
        })
    }

    pub fn accept_fut<F, U>(mut self, cb: F) -> Self
    where
        F: FnOnce(TcpStream) -> U + Send + 'static,
        U: Future<Output = ()> + 'static,
    {
        self.accepts
            .push_back(Box::new(move |tcp| -> Pin<Box<dyn Future + 'static>> {
                Box::pin(cb(tcp))
            }));
        self
    }

    pub fn run(self) -> server::Listening {
        run_server(self)
    }
}

impl TcpConn {
    pub fn read(&self) -> Vec<u8> {
        self.try_read()
            .unwrap_or_else(|e| panic!("TcpConn(addr={}) read() error: {:?}", self.addr, e))
    }

    pub fn read_timeout(&self, timeout: Duration) -> Vec<u8> {
        use linkerd2_test_util::BlockOnFor;
        current_thread::Runtime::new()
            .unwrap()
            .block_on_for(timeout, self.read_future())
            .unwrap_or_else(|e| panic!("TcpConn(addr={}) read() error: {:?}", self.addr, e))
    }

    fn read_future(&self) -> impl Future<Item = Vec<u8>, Error = io::Error> {
        println!("tcp client (addr={}): read", self.addr);
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.unbounded_send((None, tx));
        rx.map_err(|_| panic!("tcp read dropped"))
            .and_then(|res| res.map(|opt| opt.unwrap()))
    }

    pub fn try_read(&self) -> io::Result<Vec<u8>> {
        self.read_future().wait()
    }

    pub fn write<T: Into<Vec<u8>>>(&self, buf: T) {
        println!("tcp client (addr={}): write", self.addr);
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.unbounded_send((Some(buf.into()), tx));
        rx.map_err(|_| panic!("tcp write dropped"))
            .map(|rsp| assert!(rsp.unwrap().is_none()))
            .wait()
            .unwrap()
    }
}

fn run_client(addr: SocketAddr) -> TcpSender {
    let (tx, rx) = mpsc::unbounded_channel();
    let tname = format!("support tcp client (addr={})", addr);
    ::std::thread::Builder::new()
        .name(tname)
        .spawn(move || {
            let mut core =
                runtime::current_thread::Runtime::new().expect("support tcp client runtime");

            let work = async move {
                while let Some(cb) = rx.recv().await {
                    let tcp = TcpStream::connect(&addr).await.expect("tcp connect error");
                    let (tx, rx) = mpsc::unbounded_channel();
                    cb.send(tx);
                    tokio::spawn(async move {
                        while let Some((action, cb)) = rx.recv().await {
                            match action {
                                None => {
                                    let mut vec = vec![0; 1024];
                                    match tcp.read(&mut vec).await {
                                        Ok(n) => {
                                            vec.truncate(n);
                                            cb.send(Ok(Some(vec)));
                                        }
                                        Err(e) => {
                                            cb.send(Err(e));
                                            break;
                                        }
                                    }
                                }
                                Some(vec) => match tcp.write_all(vec).await {
                                    Ok(_) => cb.send(Ok(None)),
                                    Err(e) => {
                                        cb.send(Err(e));
                                        break;
                                    }
                                },
                            }
                        }
                    });
                }
            };
            core.block_on(work).unwrap();
        })
        .unwrap();

    println!("tcp client (addr={}) thread running", addr);
    tx
}

fn run_server(tcp: TcpServer) -> server::Listening {
    let (tx, rx) = shutdown_signal();
    let (started_tx, started_rx) = oneshot::channel();
    let conn_count = Arc::new(AtomicUsize::from(0));
    let srv_conn_count = Arc::clone(&conn_count);
    let any_port = SocketAddr::from(([127, 0, 0, 1], 0));
    let std_listener = StdTcpListener::bind(&any_port).expect("bind");
    let addr = std_listener.local_addr().expect("local_addr");
    let tname = format!("support tcp server (addr={})", addr);
    ::std::thread::Builder::new()
        .name(tname)
        .spawn(move || {
            let mut core =
                runtime::current_thread::Runtime::new().expect("support tcp server Runtime::new");

            let bind = TcpListener::from_std(std_listener).expect("TcpListener::from_std");

            let mut accepts = tcp.accepts;

            let listen = bind
                .incoming()
                .for_each(move |sock| {
                    let cb = accepts.pop_front().expect("no more accepts");
                    srv_conn_count.fetch_add(1, Ordering::Release);

                    let fut = cb.call_box(sock);

                    current_thread::TaskExecutor::current()
                        .execute(fut)
                        .map_err(|e| {
                            println!("tcp execute error: {:?}", e);
                            io::Error::from(io::ErrorKind::Other)
                        })
                        .map(|_| ())
                })
                .map_err(|e| panic!("tcp accept error: {}", e));

            core.spawn(listen);

            let _ = started_tx.send(());
            core.block_on(rx).unwrap();
        })
        .unwrap();

    started_rx.wait().expect("support tcp server started");

    // printlns will show if the test fails...
    println!("tcp server (addr={}): running", addr);

    server::Listening {
        addr,
        _shutdown: tx,
        conn_count,
    }
}

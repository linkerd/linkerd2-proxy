use super::*;
use std::collections::VecDeque;
use std::io;
use std::net::TcpListener as StdTcpListener;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing_futures::Instrument;

type TcpConnSender = mpsc::UnboundedSender<(
    Option<Vec<u8>>,
    oneshot::Sender<io::Result<Option<Vec<u8>>>>,
)>;

pub fn client(addr: SocketAddr) -> TcpClient {
    TcpClient { addr }
}

pub fn server() -> TcpServer {
    TcpServer {
        accepts: VecDeque::new(),
    }
}

pub struct TcpClient {
    addr: SocketAddr,
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
    task: JoinHandle<()>,
}

impl TcpClient {
    pub async fn connect(&self) -> TcpConn {
        let tcp = TcpStream::connect(&self.addr)
            .await
            .expect("tcp connect error");
        let (tx, rx) = mpsc::unbounded_channel::<(
            Option<Vec<u8>>,
            oneshot::Sender<Result<Option<Vec<u8>>, io::Error>>,
        )>();
        let span = tracing::info_span!("tcp_client", addr = %self.addr);
        tracing::info!(parent: &span, "connected");
        let task = tokio::spawn(
            async move {
                let mut rx = rx;
                let mut tcp = tcp;
                while let Some((action, cb)) = rx.recv().await {
                    match action {
                        None => {
                            let mut vec = vec![0; 1024];
                            match tcp.read(&mut vec).await {
                                Ok(n) => {
                                    vec.truncate(n);
                                    tracing::trace!(read = n, data = ?vec);
                                    let _ = cb.send(Ok(Some(vec)));
                                }
                                Err(e) => {
                                    tracing::trace!(read_error = %e);
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
                                tracing::trace!(write_error = %e);
                                cb.send(Err(e)).unwrap();
                                break;
                            }
                        },
                    }
                }
            }
            .instrument(span),
        );
        TcpConn {
            addr: self.addr,
            tx,
            task,
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
                .map(|r| r.expect("TCP server must not fail")),
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
    pub async fn read(&self) -> Vec<u8> {
        self.try_read()
            .await
            .unwrap_or_else(|e| panic!("TcpConn(addr={}) read() error: {:?}", self.addr, e))
    }

    pub async fn read_timeout(&self, timeout: Duration) -> Vec<u8> {
        tokio::time::timeout(timeout, self.read())
            .await
            .unwrap_or_else(|e| panic!("TcpConn(addr={}) read() error: {}", self.addr, e))
    }

    pub async fn try_read(&self) -> io::Result<Vec<u8>> {
        async {
            tracing::info!("try_read");
            let (tx, rx) = oneshot::channel();
            let _ = self.tx.send((None, tx));
            let res = rx.await.expect("tcp read dropped");
            res.map(|opt| opt.unwrap())
        }
        .instrument(tracing::info_span!("TcpConn::try_read", addr = %self.addr))
        .await
    }

    pub async fn write<T: Into<Vec<u8>>>(&self, buf: T) {
        let buf = buf.into();
        async {
            tracing::info!("try_read");
            let (tx, rx) = oneshot::channel();
            let _ = self.tx.send((Some(buf), tx));
            rx.await
                .map(|rsp| assert!(rsp.unwrap().is_none()))
                .expect("tcp write dropped")
        }
        .instrument(tracing::info_span!("TcpConn::write", addr = %self.addr))
        .await
    }

    pub async fn shutdown(self) {
        let Self { tx, task, .. } = self;
        drop(tx);
        task.await.unwrap()
    }
}

async fn run_server(tcp: TcpServer) -> server::Listening {
    let (drain_tx, drain) = drain::channel();
    let (started_tx, started_rx) = oneshot::channel();
    let conn_count = Arc::new(AtomicUsize::from(0));
    let srv_conn_count = Arc::clone(&conn_count);
    let any_port = SocketAddr::from(([127, 0, 0, 1], 0));
    let std_listener = StdTcpListener::bind(&any_port).expect("bind");
    let addr = std_listener.local_addr().expect("local_addr");
    let task = tokio::spawn(
        cancelable(drain.clone(), async move {
            let mut accepts = tcp.accepts;
            std_listener
                .set_nonblocking(true)
                .expect("socket must be able to set nonblocking");
            let listener = TcpListener::from_std(std_listener).expect("TcpListener::from_std");

            let _ = started_tx.send(());
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
        })
        .instrument(tracing::info_span!("tcp_server", %addr)),
    );

    started_rx.await.expect("support tcp server started");

    tracing::info!(%addr, "tcp server running");

    server::Listening {
        addr,
        drain: drain_tx,
        conn_count,
        task: Some(task),
    }
}

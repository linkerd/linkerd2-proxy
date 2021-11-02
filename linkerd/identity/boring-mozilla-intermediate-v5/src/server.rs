use boring::ssl;
use linkerd_identity::Name;
use tokio::sync::watch;

#[derive(Clone)]
pub struct Server {
    name: Name,
    rx: watch::Receiver<ssl::SslAcceptor>,
}

pub struct TerminateFuture {}

pub struct ServerIo {}

impl Server {
    pub(crate) fn new(name: Name, rx: watch::Receiver<ssl::SslAcceptor>) -> Self {
        Self { name, rx }
    }
}

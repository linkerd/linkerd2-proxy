use boring::ssl;
use tokio::sync::watch;

#[derive(Clone)]
pub struct Receiver {
    client_rx: watch::Receiver<ssl::SslConnector>,
    server_rx: watch::Receiver<ssl::SslAcceptor>,
}

impl Receiver {
    pub(crate) fn new(
        client_rx: watch::Receiver<ssl::SslConnector>,
        server_rx: watch::Receiver<ssl::SslAcceptor>,
    ) -> Self {
        Self {
            client_rx,
            server_rx,
        }
    }
}

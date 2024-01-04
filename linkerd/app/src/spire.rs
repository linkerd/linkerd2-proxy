pub use linkerd_app_core::identity::spire_client;
use linkerd_app_core::{exp_backoff::ExponentialBackoff, Error};
use std::sync::Arc;
use tokio::net::UnixStream;
use tokio::sync::watch;
use tonic::transport::{Endpoint, Uri};

const UNIX_PREFIX: &str = "unix:";
const TONIC_DEFAULT_URI: &str = "http://[::]:50051";

#[derive(Clone, Debug)]
pub struct Config {
    pub(crate) socket_addr: Arc<String>,
    pub(crate) backoff: ExponentialBackoff,
}

// Connects to SPIRE workload API via Unix Domain Socket
pub struct Client {
    config: Config,
}

// === impl Client ===

impl From<Config> for Client {
    fn from(config: Config) -> Self {
        Self { config }
    }
}

impl tower::Service<()> for Client {
    type Response = tonic::Response<watch::Receiver<spire_client::SvidUpdate>>;
    type Error = Error;
    type Future = futures::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: ()) -> Self::Future {
        let socket = self.config.socket_addr.clone();
        let backoff = self.config.backoff;
        Box::pin(async move {
            // Strip the 'unix:' prefix for tonic compatibility.
            let stripped_path = socket
                .strip_prefix(UNIX_PREFIX)
                .unwrap_or(socket.as_str())
                .to_string();

            // We will ignore this uri because uds do not use it
            // if your connector does use the uri it will be provided
            // as the request to the `MakeConnection`.
            let chan = Endpoint::try_from(TONIC_DEFAULT_URI)?
                .connect_with_connector(tower::util::service_fn(move |_: Uri| {
                    UnixStream::connect(stripped_path.clone())
                }))
                .await?;

            let api = spire_client::Api::watch(chan, backoff);
            let receiver = api.spawn_watch(()).await?;

            Ok(receiver)
        })
    }
}

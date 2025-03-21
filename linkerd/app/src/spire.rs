use linkerd_app_core::{exp_backoff::ExponentialBackoff, Error};
use std::sync::Arc;
use tokio::sync::watch;

pub use linkerd_app_core::identity::client::spire as client;

#[cfg(target_os = "linux")]
const UNIX_PREFIX: &str = "unix:";
#[cfg(target_os = "linux")]
const TONIC_DEFAULT_URI: &str = "http://[::]:50051";

#[derive(Clone, Debug)]
pub struct Config {
    pub socket_addr: Arc<String>,
    pub backoff: ExponentialBackoff,
}

// Connects to SPIRE workload API via Unix Domain Socket
pub struct Client {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    config: Config,
}

// === impl Client ===

#[cfg(target_os = "linux")]
impl From<Config> for Client {
    fn from(config: Config) -> Self {
        Self { config }
    }
}

#[cfg(not(target_os = "linux"))]
impl From<Config> for Client {
    fn from(_: Config) -> Self {
        panic!("Spire is supported on Linux only")
    }
}

#[cfg(target_os = "linux")]
impl tower::Service<()> for Client {
    type Response = tonic::Response<watch::Receiver<client::SvidUpdate>>;
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
            use tokio::net::UnixStream;
            use tonic::transport::{Endpoint, Uri};

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
                    use futures::TryFutureExt;
                    UnixStream::connect(stripped_path.clone()).map_ok(hyper_util::rt::TokioIo::new)
                }))
                .await?;

            let api = client::Api::watch(chan, backoff);
            let receiver = api.spawn_watch(()).await?;

            Ok(receiver)
        })
    }
}

#[cfg(not(target_os = "linux"))]
impl tower::Service<()> for Client {
    type Response = tonic::Response<watch::Receiver<client::SvidUpdate>>;
    type Error = Error;
    type Future = futures::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        unimplemented!("Spire is supported on Linux only")
    }

    fn call(&mut self, _req: ()) -> Self::Future {
        unimplemented!("Spire is supported on Linux only")
    }
}

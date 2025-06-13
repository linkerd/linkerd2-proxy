use linkerd_app_core::{exp_backoff::ExponentialBackoff, Error};
use std::sync::Arc;
use tokio::sync::watch;

pub use linkerd_app_core::identity::client::spire as client;

const TONIC_DEFAULT_URI: &str = "http://[::]:50051";

#[derive(Clone, Debug)]
pub struct Config {
    pub workload_api_addr: Arc<String>,
    pub backoff: ExponentialBackoff,
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
        let addr = self.config.workload_api_addr.clone();
        let backoff = self.config.backoff;
        Box::pin(async move {
            use hyper_util::rt::TokioIo;
            use tonic::transport::{Endpoint, Uri};

            // We will ignore this uri because uds do not use it
            // if your connector does use the uri it will be provided
            // as the request to the `MakeConnection`.
            let chan = Endpoint::try_from(TONIC_DEFAULT_URI)?
                .connect_with_connector(tower::util::service_fn(move |_: Uri| {
                    #[cfg(unix)]
                    {
                        use tokio::net::UnixStream;
                        const UNIX_PREFIX: &str = "unix:";
                        use futures::TryFutureExt;

                        // Strip the 'unix:' prefix for tonic compatibility.
                        let stripped_path = addr
                            .strip_prefix(UNIX_PREFIX)
                            .unwrap_or(addr.as_str())
                            .to_string();

                        UnixStream::connect(stripped_path.clone()).map_ok(TokioIo::new)
                    }

                    #[cfg(windows)]
                    {
                        use tokio::net::windows::named_pipe;
                        let named_pipe_path = addr.clone();
                        let client = named_pipe::ClientOptions::new()
                            .open(named_pipe_path.as_str())
                            .map(TokioIo::new);

                        futures::future::ready(client)
                    }
                }))
                .await?;

            let api = client::Api::watch(chan, backoff);
            let receiver = api.spawn_watch(()).await?;

            Ok(receiver)
        })
    }
}

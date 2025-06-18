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
            use tonic::transport::{Endpoint, Uri};

            // We will ignore this uri because uds do not use it
            // if your connector does use the uri it will be provided
            // as the request to the `MakeConnection`.
            let chan = Endpoint::try_from(TONIC_DEFAULT_URI)?
                .connect_with_connector(tower::util::service_fn(move |_: Uri| {
                    #[cfg(unix)]
                    {
                        use futures::TryFutureExt;

                        // The 'unix:' scheme must be stripped from socket paths.
                        let path = addr.strip_prefix("unix:").unwrap_or(addr.as_str());

                        tokio::net::UnixStream::connect(path.to_string())
                            .map_ok(hyper_util::rt::TokioIo::new)
                    }

                    #[cfg(windows)]
                    {
                        use tokio::net::windows::named_pipe;
                        let named_pipe_path = addr.clone();
                        let client = named_pipe::ClientOptions::new()
                            .open(named_pipe_path.as_str())
                            .map(hyper_util::rt::TokioIo::new);

                        futures::future::ready(client)
                    }

                    #[cfg(not(any(unix, windows)))]
                    {
                        compile_error!("Spire is supported only on Windows and Unix systems.");
                        futures::future::pending()
                    }
                }))
                .await?;

            let api = client::Api::watch(chan, backoff);
            let receiver = api.spawn_watch(()).await?;

            Ok(receiver)
        })
    }
}

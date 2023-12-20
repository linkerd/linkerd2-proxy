use crate::api::{Api, SvidUpdate};
use linkerd_error::Error;
use linkerd_exp_backoff::ExponentialBackoff;
use std::sync::Arc;
use tokio::net::UnixStream;
use tokio::sync::watch;
use tonic::transport::{Endpoint, Uri};

const UNIX_PREFIX: &str = "unix:";
const TONIC_DEFAULT_URI: &str = "http://[::]:50051";

#[derive(Clone, Debug)]
pub struct Client {
    socket: Arc<String>,
    backoff: ExponentialBackoff,
}

impl Client {
    pub fn new(socket: Arc<String>, backoff: ExponentialBackoff) -> Self {
        Self { socket, backoff }
    }
}

// === impl Client ===

impl tower::Service<()> for Client {
    type Response = watch::Receiver<SvidUpdate>;
    type Error = Error;
    type Future = futures::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: ()) -> Self::Future {
        let socket = self.socket.clone();
        let backoff = self.backoff;
        Box::pin(async move {
            //spiffe::workload_api::client::WorkloadApiClient
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

            let api = Api::watch(chan, backoff);
            let receiver = api.spawn_watch(()).await?.into_inner();

            Ok(receiver)
        })
    }
}

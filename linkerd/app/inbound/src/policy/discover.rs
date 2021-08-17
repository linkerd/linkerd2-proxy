use futures::prelude::*;
use linkerd2_proxy_api::inbound::{
    self as api, inbound_server_policies_client::InboundServerPoliciesClient as ApiClient,
};
use linkerd_app_core::{
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    proxy::http,
    svc::Service,
    Error, Recover,
};
use linkerd_tonic_watch::StreamWatch;

#[derive(Clone, Debug)]
pub(super) struct Discover<S> {
    workload: String,
    client: ApiClient<S>,
}

#[derive(Clone)]
pub(super) struct GrpcRecover(ExponentialBackoff);

pub(super) type Watch<S> = StreamWatch<GrpcRecover, Discover<S>>;

impl<S> Discover<S>
where
    S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error> + Clone,
    S::ResponseBody: http::HttpBody<Error = Error> + Send + Sync + 'static,
{
    pub(super) fn new(workload: String, client: S) -> Self {
        Self {
            workload,
            client: ApiClient::new(client),
        }
    }

    pub(super) fn into_watch(self, backoff: ExponentialBackoff) -> Watch<S> {
        StreamWatch::new(GrpcRecover(backoff), self)
    }
}

impl<S> Service<u16> for Discover<S>
where
    S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
    S: Clone + Send + Sync + 'static,
    S::Future: Send + 'static,
    S::ResponseBody: http::HttpBody<Error = Error> + Send + Sync + 'static,
{
    type Response =
        tonic::Response<futures::stream::BoxStream<'static, Result<api::Server, tonic::Status>>>;
    type Error = tonic::Status;
    type Future = futures::future::BoxFuture<'static, Result<Self::Response, tonic::Status>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, port: u16) -> Self::Future {
        let req = api::PortSpec {
            port: port.into(),
            workload: self.workload.clone(),
        };
        let mut client = self.client.clone();
        Box::pin(async move {
            let s = client.watch_port(tonic::Request::new(req)).await?;
            Ok(s.map(|b| b.boxed()))
        })
    }
}

// === impl GrpcRecover ===

impl Recover<tonic::Status> for GrpcRecover {
    type Backoff = ExponentialBackoffStream;

    fn recover(&self, status: tonic::Status) -> Result<Self::Backoff, tonic::Status> {
        if status.code() == tonic::Code::InvalidArgument
            || status.code() == tonic::Code::FailedPrecondition
        {
            return Err(status);
        }

        tracing::trace!(%status, "Recovering");
        Ok(self.0.stream())
    }
}

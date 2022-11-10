use futures::prelude::*;
use linkerd2_proxy_api::outbound::{
    self as api, outbound_policies_client::OutboundPoliciesClient as Client,
};
use linkerd_app_core::{
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    proxy::http,
    svc::Service,
    Error, Recover, Result,
};
use linkerd_client_policy::ClientPolicy;
use linkerd_tonic_watch::StreamWatch;
use std::{
    net::SocketAddr,
    task::{Context, Poll},
    time,
};

#[derive(Clone, Debug)]
pub(super) struct Api<S> {
    workload: Arc<str>,
    client: Client<S>,
}

#[derive(Clone)]
pub(super) struct GrpcRecover(ExponentialBackoff);

pub(super) type Watch<S> = StreamWatch<GrpcRecover, Api<S>>;

/// If an invalid policy is encountered, then this will be updated to hold a
/// default, invalid policy.
static INVALID_POLICY: once_cell::sync::Lazy<ClientPolicy> = once_cell::sync::Lazy::new();

impl<S> Api<S>
where
    S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error> + Clone,
    S::ResponseBody:
        http::HttpBody<Data = tonic::codegen::Bytes, Error = Error> + Default + Send + 'static,
{
    pub(super) fn new(workload: String, client: S) -> Self {
        Self {
            workload: workload.into(),
            client: Client::new(client),
        }
    }

    pub(super) fn into_watch(self, backoff: ExponentialBackoff) -> Watch<S> {
        StreamWatch::new(GrpcRecover(backoff), self)
    }
}

impl<S> Service<SocketAddr> for Api<S>
where
    S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
    S: Clone + Send + Sync + 'static,
    S::ResponseBody:
        http::HttpBody<Data = tonic::codegen::Bytes, Error = Error> + Default + Send + 'static,
    S::Future: Send + 'static,
{
    type Response =
        tonic::Response<futures::stream::BoxStream<'static, Result<ClientPolicy, tonic::Status>>>;
    type Error = tonic::Status;
    type Future = futures::future::BoxFuture<'static, Result<Self::Response, tonic::Status>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, addr: SocketAddr) -> Self::Future {
        let target = api::TargetSpec {
            ip: addr.ip().into(),
            port: addr.port() as u32,
            workload: self.workload.to_owned(),
        };
        let mut client = self.client.clone();
        Box::pin(async move {
            let rsp = client.watch(target).await?;
            Ok(rsp.map(|updates| {
                updates
                    .map_ok(move |up| {
                        // If the server returned an invalid client policy, we
                        // default to using an invalid policy. Currently, this
                        // doesn't cause an internal error, because we don't have
                        // filters in client policy rules yet...
                        // TODO(eliza): this should get an internal error filter...
                        let policy = ClientPolicy::try_from(up).unwrap_or_else(|error| {
                            tracing::warn!(%error, "Client policy misconfigured");
                            INVALID_POLICY.get().clone()
                        });
                        tracing::debug!(?policy);
                        policy
                    })
                    .boxed()
            }))
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

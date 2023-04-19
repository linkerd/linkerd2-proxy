use futures::prelude::*;
use linkerd2_proxy_api::outbound::{
    self as api, outbound_policies_client::OutboundPoliciesClient as Client,
};
use linkerd_app_core::{
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    proxy::http,
    svc::Service,
    Addr, Error, Recover, Result,
};
use linkerd_proxy_client_policy::ClientPolicy;
use linkerd_tonic_watch::StreamWatch;
use std::{sync::Arc, time};

#[derive(Clone, Debug)]
pub(crate) struct Api<S> {
    workload: Arc<str>,
    detect_timeout: time::Duration,
    client: Client<S>,
}

#[derive(Clone)]
pub(crate) struct GrpcRecover(ExponentialBackoff);
pub(crate) type Watch<S> = StreamWatch<GrpcRecover, Api<S>>;

/// If an invalid policy is encountered, then this will be updated to hold a
/// default, invalid policy.
static INVALID_POLICY: once_cell::sync::OnceCell<ClientPolicy> = once_cell::sync::OnceCell::new();

impl<S> Api<S>
where
    S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error> + Clone,
    S::ResponseBody:
        http::HttpBody<Data = tonic::codegen::Bytes, Error = Error> + Default + Send + 'static,
{
    pub(crate) fn new(workload: Arc<str>, detect_timeout: time::Duration, client: S) -> Self {
        Self {
            workload,
            detect_timeout,
            client: Client::new(client),
        }
    }

    pub(crate) fn into_watch(self, backoff: ExponentialBackoff) -> Watch<S> {
        StreamWatch::new(GrpcRecover(backoff), self)
    }
}

impl<S> Service<Addr> for Api<S>
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

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, addr: Addr) -> Self::Future {
        let req = {
            let target = match addr {
                Addr::Name(ref name) => api::traffic_spec::Target::Authority(name.to_string()),
                Addr::Socket(sock) => api::traffic_spec::Target::Addr(sock.into()),
            };
            api::TrafficSpec {
                source_workload: self.workload.as_ref().to_string(),
                target: Some(target),
            }
        };
        let detect_timeout = self.detect_timeout;
        let mut client = self.client.clone();
        Box::pin(async move {
            let rsp = client.watch(tonic::Request::new(req)).await?;
            Ok(rsp.map(|updates| {
                updates
                    .map_ok(move |up| {
                        // If the server returned an invalid client policy, we
                        // default to using an invalid policy that causes all
                        // requests to report an internal error.
                        let policy = ClientPolicy::try_from(up).unwrap_or_else(|error| {
                            tracing::warn!(%error, "Client policy misconfigured");
                            INVALID_POLICY
                                .get_or_init(|| ClientPolicy::invalid(detect_timeout))
                                .clone()
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
        match status.code() {
            // Non-retryable
            tonic::Code::InvalidArgument | tonic::Code::FailedPrecondition => Err(status),
            // Indicates no policy for this target
            tonic::Code::NotFound | tonic::Code::Unimplemented => Err(status),
            _ => {
                tracing::debug!(%status, "Recovering");
                Ok(self.0.stream())
            }
        }
    }
}

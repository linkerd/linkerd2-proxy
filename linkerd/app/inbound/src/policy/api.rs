use futures::prelude::*;
use linkerd2_proxy_api::inbound::{
    self as api, inbound_server_policies_client::InboundServerPoliciesClient as Client,
};
use linkerd_app_core::{
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    proxy::http,
    svc::Service,
    Error, Recover, Result,
};
use linkerd_server_policy::ServerPolicy;
use linkerd_tonic_watch::StreamWatch;
use std::{sync::Arc, time};

#[derive(Clone, Debug)]
pub(super) struct Api<S> {
    workload: Arc<str>,
    detect_timeout: time::Duration,
    client: Client<S>,
}

#[derive(Clone)]
pub(super) struct GrpcRecover(ExponentialBackoff);

pub(super) type Watch<S> = StreamWatch<GrpcRecover, Api<S>>;

/// If an invalid policy is encountered, then this will be updated to hold a
/// default, invalid policy.
static INVALID_POLICY: once_cell::sync::OnceCell<ServerPolicy> = once_cell::sync::OnceCell::new();

impl<S> Api<S>
where
    S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error> + Clone,
    S::ResponseBody:
        http::HttpBody<Data = tonic::codegen::Bytes, Error = Error> + Default + Send + 'static,
{
    pub(super) fn new(workload: Arc<str>, detect_timeout: time::Duration, client: S) -> Self {
        Self {
            workload,
            detect_timeout,
            client: Client::new(client),
        }
    }

    pub(super) fn into_watch(self, backoff: ExponentialBackoff) -> Watch<S> {
        StreamWatch::new(GrpcRecover(backoff), self)
    }
}

impl<S> Service<u16> for Api<S>
where
    S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
    S: Clone + Send + Sync + 'static,
    S::ResponseBody:
        http::HttpBody<Data = tonic::codegen::Bytes, Error = Error> + Default + Send + 'static,
    S::Future: Send + 'static,
{
    type Response =
        tonic::Response<futures::stream::BoxStream<'static, Result<ServerPolicy, tonic::Status>>>;
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
            workload: (*self.workload).to_owned(),
        };
        let detect_timeout = self.detect_timeout;
        let mut client = self.client.clone();
        Box::pin(async move {
            let rsp = client.watch_port(tonic::Request::new(req)).await?;
            Ok(rsp.map(|updates| {
                updates
                    .map_ok(move |up| {
                        // If the server returned an invalid server policy, we
                        // default to using an invalid policy that causes all
                        // requests to report an internal error.
                        let policy = ServerPolicy::try_from(up).unwrap_or_else(|error| {
                            tracing::warn!(%error, "Server misconfigured");
                            INVALID_POLICY
                                .get_or_init(|| ServerPolicy::invalid(detect_timeout))
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
        if status.code() == tonic::Code::InvalidArgument
            || status.code() == tonic::Code::FailedPrecondition
        {
            return Err(status);
        }

        tracing::trace!(%status, "Recovering");
        Ok(self.0.stream())
    }
}

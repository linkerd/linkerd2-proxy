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

#[derive(Clone, Debug)]
pub(super) struct Api<S> {
    workload: String,
    client: Client<S>,
}

#[derive(Clone)]
pub(super) struct GrpcRecover(ExponentialBackoff);

pub(super) type Watch<S> = StreamWatch<GrpcRecover, Api<S>>;

impl<S> Api<S>
where
    S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error> + Clone,
    S::ResponseBody:
        http::HttpBody<Data = tonic::codegen::Bytes, Error = Error> + Default + Send + 'static,
{
    pub(super) fn new(workload: String, client: S) -> Self {
        Self {
            workload,
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
            workload: self.workload.clone(),
        };
        let mut client = self.client.clone();
        Box::pin(async move {
            let rsp = client.watch_port(tonic::Request::new(req)).await?;
            Ok(rsp.map(|updates| {
                updates
                    .map(|up| {
                        // FIXME(ver): If we get an invalid policy, we can't
                        // pretend we didn't get it -- it might mean that the
                        // server has tried to configure a policy that we don't
                        // support!
                        let policy = ServerPolicy::try_from(up?).map_err(|e| {
                            tonic::Status::invalid_argument(&*format!(
                                "received invalid policy: {}",
                                e
                            ))
                        })?;
                        tracing::debug!(?policy);
                        Ok(policy)
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

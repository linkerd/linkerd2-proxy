use super::{api::Api, Receiver};

use linkerd_app_core::{
    control, dns, identity, metrics,
    svc::{self, NewService},
    transport::OrigDstAddr,
    Error,
};
use std::sync::Arc;
use tower::ServiceExt;

#[derive(Clone, Debug)]
pub struct Config {
    pub control: control::Config,
    pub workload: Arc<str>,
}

impl Config {
    pub(crate) fn build(
        self,
        dns: dns::Resolver,
        metrics: metrics::ControlHttp,
        identity: identity::NewClient,
    ) -> impl svc::Service<OrigDstAddr, Response = Receiver, Future = impl Send, Error = Error>
           + Clone
           + Send
           + Sync
           + 'static {
        let Self { control, workload } = self;
        let backoff = control.connect.backoff;
        let client = control.build(dns, metrics, identity).new_service(());
        Api::new(workload, client)
            .into_watch(backoff)
            .map_response(tonic::Response::into_inner)
            .map_err(Error::from)
    }
}

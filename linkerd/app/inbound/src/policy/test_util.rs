use crate::policy::*;
use linkerd_app_core::{exp_backoff::ExponentialBackoff, proxy::http, Error};
use linkerd_proxy_server_policy::ServerPolicy;

pub type Store = super::Store<Unreachable>;

#[derive(Copy, Clone, Debug)]
pub struct Unreachable(());

// === impl Store ===

impl Store {
    /// Returns a new `Store` that always returns the given `ServerPolicy` for each port.
    ///
    /// Calls to the API panic.
    pub(crate) fn fixed_for_test(ports: impl IntoIterator<Item = (u16, ServerPolicy)>) -> Self {
        Self::spawn_fixed(
            std::time::Duration::MAX,
            ports,
            api::Api::new("test".into(), std::time::Duration::MAX, Unreachable(()))
                .into_watch(ExponentialBackoff::default()),
        )
    }
}

// === impl Unreachable ===

impl tonic::client::GrpcService<tonic::body::BoxBody> for Unreachable {
    type ResponseBody = linkerd_app_core::control::RspBody;
    type Error = Error;
    type Future = futures::future::Pending<Result<http::Response<Self::ResponseBody>, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Error>> {
        unreachable!()
    }

    fn call(&mut self, _req: http::Request<tonic::body::BoxBody>) -> Self::Future {
        unreachable!()
    }
}

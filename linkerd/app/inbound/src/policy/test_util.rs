use super::Store;
use crate::policy::*;
use linkerd_app_core::svc;
use linkerd_proxy_server_policy::ServerPolicy;

#[derive(Copy, Clone, Debug)]
pub struct Unreachable(());

// === impl Store ===

impl<D> Store<D>
where
    D: svc::Service<
        u16,
        Response = tonic::Response<watch::Receiver<ServerPolicy>>,
        Error = tonic::Status,
    >,
    D: Clone + Send + Sync + 'static,
    D::Future: Send,
{
    /// Returns a new `Store` that always returns the given `ServerPolicy` for each port.
    ///
    /// Calls to the API panic.
    pub fn for_test(ports: impl IntoIterator<Item = (u16, ServerPolicy)>, api: D) -> Self {
        Self::spawn_fixed(std::time::Duration::MAX, ports, api)
    }
}

impl Store<Unreachable> {
    /// Returns a new `Store` that always returns the given `ServerPolicy` for each port.
    ///
    /// Calls to the API panic.
    pub fn fixed_for_test(ports: impl IntoIterator<Item = (u16, ServerPolicy)>) -> Self {
        Self::for_test(ports, Unreachable(()))
    }
}

// === impl Unreachable ===

impl<T> svc::Service<T> for Unreachable {
    type Response = tonic::Response<watch::Receiver<ServerPolicy>>;
    type Error = tonic::Status;
    type Future = futures::future::Pending<
        Result<tonic::Response<watch::Receiver<ServerPolicy>>, tonic::Status>,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), tonic::Status>> {
        unreachable!()
    }

    fn call(&mut self, _req: T) -> Self::Future {
        unreachable!()
    }
}

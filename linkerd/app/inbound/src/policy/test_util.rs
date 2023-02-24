use super::Store;
use crate::policy::*;
use linkerd_app_core::svc;
use linkerd_proxy_server_policy::ServerPolicy;
use tower_test::mock;

#[derive(Copy, Clone, Debug)]
pub struct Unreachable(());

// === impl Store ===

impl
    Store<
        svc::MapErr<
            fn(Error) -> tonic::Status,
            mock::Mock<u16, tonic::Response<watch::Receiver<ServerPolicy>>>,
        >,
    >
{
    /// Returns a new `Store` that always returns the given `ServerPolicy` for each port.
    ///
    /// Calls to the API panic.
    pub fn for_test(
        ports: impl IntoIterator<Item = (u16, ServerPolicy)>,
    ) -> (
        Self,
        mock::Handle<u16, tonic::Response<watch::Receiver<ServerPolicy>>>,
    ) {
        let (disco, handle) = mock::pair();
        let disco = svc::MapErr::new(
            disco,
            (|_: Error| tonic::Status::internal("invalid")) as fn(_) -> _,
        );
        let store = Self::spawn_fixed(std::time::Duration::MAX, ports, disco);
        (store, handle)
    }
}

impl Store<Unreachable> {
    /// Returns a new `Store` that always returns the given `ServerPolicy` for each port.
    ///
    /// Calls to the API panic.
    pub fn fixed_for_test(ports: impl IntoIterator<Item = (u16, ServerPolicy)>) -> Self {
        Self::spawn_fixed(std::time::Duration::MAX, ports, Unreachable(()))
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

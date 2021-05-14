use linkerd_app_core::{
    proxy::{
        core::Resolve,
        discover::{self, Buffer},
    },
    svc::{layer, NewService},
};
use std::time::Duration;

pub fn layer<T, R, N>(
    resolve: R,
    watchdog: Duration,
) -> impl layer::Layer<N, Service = Buffer<discover::Stack<N, R, R::Endpoint>>> + Clone
where
    T: Clone + Send + std::fmt::Debug,
    R: Resolve<T> + Clone,
    R::Resolution: Send,
    R::Future: Send,
    N: NewService<R::Endpoint>,
{
    const ENDPOINT_BUFFER_CAPACITY: usize = 1_000;

    layer::mk(move |new_endpoint| {
        Buffer::new(
            ENDPOINT_BUFFER_CAPACITY,
            watchdog,
            discover::resolve(new_endpoint, resolve.clone()),
        )
    })
}

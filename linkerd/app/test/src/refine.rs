use futures::future;
use linkerd2_app_core::{dns, Error};
use std::task::{Context, Poll};

pub fn never() -> Never {
    Never(())
}

#[derive(Debug, Clone)]
pub struct Never(());

impl tower::Service<dns::Name> for Never {
    type Response = dns::Name;
    type Future = future::Ready<Result<dns::Name, Error>>;
    type Error = Error;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: dns::Name) -> Self::Future {
        panic!(
            "DNS refinement should not be used in this test, \
            but we tried to refine the name '{}'",
            name
        )
    }
}

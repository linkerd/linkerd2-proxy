use crate::{NewService, Service};
use futures::future;
use std::task::{Context, Poll};

#[derive(Debug, Clone)]
pub struct Join<A, B> {
    a: A,
    b: B,
}

impl<A, B: Clone> Join<A, B> {
    pub fn layer(b: B) -> impl crate::layer::Layer<A, Service = Join<A, B>> + Clone {
        crate::layer::mk(move |a| Join { a, b: b.clone() })
    }

    pub fn new(a: A, b: B) -> Self {
        Self { a, b }
    }
}

impl<A, B, T> NewService<T> for Join<A, B>
where
    A: NewService<T>,
    B: NewService<T>,
    T: Clone,
{
    type Service = Join<A::Service, B::Service>;
    fn new_service(&self, target: T) -> Self::Service {
        let a = self.a.new_service(target.clone());
        let b = self.b.new_service(target);
        Join { a, b }
    }
}

impl<A, B, Req> Service<Req> for Join<A, B>
where
    A: Service<Req>,
    // XXX(eliza): might be marginally more convenient to give this a built-in
    // map_err?
    B: Service<Req, Error = A::Error>,
    Req: Clone,
{
    type Response = (A::Response, B::Response);
    type Error = A::Error;
    type Future = future::TryJoin<A::Future, B::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // always poll both services, even if one is unready.
        let a_poll = self.a.poll_ready(cx);
        futures::ready!(self.b.poll_ready(cx))?;
        a_poll
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let a = self.a.call(req.clone());
        let b = self.b.call(req);
        future::try_join(a, b)
    }
}

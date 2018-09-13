use futures::Poll;
use http;
use std::marker::PhantomData;

use svc;

/// Layers `AddExtension` onto a client.
#[derive(Copy, Debug)]
pub struct Layer<T, A>(PhantomData<fn() -> (T, A)>);

/// Wraps clients to add the target to each request's extensions.
#[derive(Debug)]
pub struct Make<T, A, N>(N, PhantomData<fn() -> (T, A)>);

/// Clones a `T`-typed target into each request's extensions.
#[derive(Clone, Debug)]
pub struct AddExtension<T, S> {
    target: T,
    inner: S,
}

// === Layer ===

pub fn layer<T: Clone + Send + Sync + 'static, B>() -> Layer<T, B> {
    Layer(PhantomData)
}

impl<T, A, N> svc::Layer<N> for Layer<T, A>
where
    N: svc::MakeClient<T>,
    N::Client: svc::Service<Request = http::Request<A>>,
    T: Clone + Send + Sync + 'static,
{
    type Bound = Make<T, A, N>;

    fn bind(&self, next: N) -> Self::Bound {
        Make(next, PhantomData)
    }
}

impl<T, A> Clone for Layer<T, A> {
    fn clone(&self) -> Self {
        Layer(PhantomData)
    }
}

// === Make ===

impl<T, A, N> svc::MakeClient<T> for Make<T, A, N>
where
    N: svc::MakeClient<T>,
    N::Client: svc::Service<Request = http::Request<A>>,
    T: Clone + Send + Sync + 'static,
{
    type Client = AddExtension<T, N::Client>;
    type Error = N::Error;

    fn make_client(&self, target: &T) -> Result<Self::Client, Self::Error> {
        let inner = self.0.make_client(target)?;
        Ok(AddExtension {
            target: target.clone(),
            inner,
        })
    }
}

impl<T, A, N: Clone> Clone for Make<T, A, N> {
    fn clone(&self) -> Self {
        Make(self.0.clone(), PhantomData)
    }
}

// === AddExtension ===

impl<T, S, A> svc::Service for AddExtension<T, S>
where
    S: svc::Service<Request = http::Request<A>>,
    T: Clone + Send + Sync + 'static,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), S::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut req: S::Request) -> S::Future {
        req.extensions_mut().insert(self.target.clone());
        self.inner.call(req)
    }
}

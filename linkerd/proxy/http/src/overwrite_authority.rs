use super::h1;
use futures::{try_ready, Future, Poll};
use http;
use http::header::AsHeaderName;
use http::uri::Authority;
use tracing::trace;

pub trait ExtractAuthority<T> {
    fn extract(&self, target: &T) -> Option<Authority>;
}

#[derive(Debug, Clone)]
pub struct Layer<E, H> {
    extractor: E,
    headers_to_strip: Vec<H>,
}

#[derive(Clone, Debug)]
pub struct MakeSvc<E, H, M> {
    extractor: E,
    headers_to_strip: Vec<H>,
    inner: M,
}

pub struct MakeSvcFut<M, H> {
    authority: Option<Authority>,
    headers_to_strip: Vec<H>,
    inner: M,
}

#[derive(Clone, Debug)]
pub struct Service<S, H> {
    authority: Option<Authority>,
    headers_to_strip: Vec<H>,
    inner: S,
}

// === impl Layer ===

pub fn layer<E, H>(extractor: E, headers_to_strip: Vec<H>) -> Layer<E, H>
where
    E: Clone,
    H: AsHeaderName + Clone,
{
    Layer {
        extractor,
        headers_to_strip,
    }
}

impl<E, H, M> tower::layer::Layer<M> for Layer<E, H>
where
    E: Clone,
    H: AsHeaderName + Clone,
{
    type Service = MakeSvc<E, H, M>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeSvc {
            extractor: self.extractor.clone(),
            headers_to_strip: self.headers_to_strip.clone(),
            inner,
        }
    }
}

impl<E, H, T, M> tower::Service<T> for MakeSvc<E, H, M>
where
    T: Clone + Send + Sync + 'static,
    M: tower::Service<T>,
    E: ExtractAuthority<T>,
    E: Clone,
    H: AsHeaderName + Clone,
{
    type Response = Service<M::Response, H>;
    type Error = M::Error;
    type Future = MakeSvcFut<M::Future, H>;

    fn poll_ready(&mut self) -> Poll<(), M::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, t: T) -> Self::Future {
        let authority = self.extractor.extract(&t);
        let inner = self.inner.call(t);
        MakeSvcFut {
            authority,
            headers_to_strip: self.headers_to_strip.clone(),
            inner,
        }
    }
}

impl<F, H> Future for MakeSvcFut<F, H>
where
    F: Future,
    H: AsHeaderName + Clone,
{
    type Item = Service<F::Item, H>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        Ok(Service {
            authority: self.authority.clone(),
            headers_to_strip: self.headers_to_strip.clone(),
            inner,
        }
        .into())
    }
}

// === impl Service ===

impl<S, H, B> tower::Service<http::Request<B>> for Service<S, H>
where
    S: tower::Service<http::Request<B>>,
    H: AsHeaderName + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        if let Some(new_authority) = self.authority.clone() {
            for h in self.headers_to_strip.iter() {
                if let Some(orig_header_val) = req.headers_mut().remove(h.clone()) {
                    trace!(
                        "Removed original {:?} header {:?}",
                        h.as_str(),
                        orig_header_val
                    );
                };
            }

            trace!(%new_authority, "Overwriting authority");
            h1::set_authority(req.uri_mut(), new_authority.clone());
        }

        self.inner.call(req)
    }
}

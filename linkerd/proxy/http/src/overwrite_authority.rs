use super::h1;
use futures::{try_ready, Future, Poll};
use http;
use http::header::HOST;
use http::header::{HeaderValue, IntoHeaderName};
use http::uri::Authority;
use tracing::trace;

pub trait ExtractAuthority<T> {
    fn extract(&self, target: &T) -> Option<Authority>;
}

pub trait ShouldOverwriteAuthority {
    fn should_overwrite_authority(&self) -> bool;
}

#[derive(Debug, Clone)]
pub struct Layer<E, H> {
    extractor: E,
    canonical_dst_header: H,
}

#[derive(Clone, Debug)]
pub struct MakeSvc<E, H, M> {
    extractor: E,
    canonical_dst_header: H,
    inner: M,
}

pub struct MakeSvcFut<M, H> {
    authority: Option<Authority>,
    canonical_dst_header: H,
    inner: M,
}

#[derive(Clone, Debug)]
pub struct Service<S, H> {
    authority: Option<Authority>,
    canonical_dst_header: H,
    inner: S,
}

// === impl Layer ===

pub fn layer<E, H>(extractor: E, canonical_dst_header: H) -> Layer<E, H>
where
    E: Clone,
    H: IntoHeaderName + Clone,
{
    Layer {
        extractor,
        canonical_dst_header,
    }
}

impl<E, H, M> tower::layer::Layer<M> for Layer<E, H>
where
    E: Clone,
    H: IntoHeaderName + Clone,
{
    type Service = MakeSvc<E, H, M>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeSvc {
            extractor: self.extractor.clone(),
            canonical_dst_header: self.canonical_dst_header.clone(),
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
    H: IntoHeaderName + Clone,
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
            canonical_dst_header: self.canonical_dst_header.clone(),
            inner,
        }
    }
}

impl<F, H> Future for MakeSvcFut<F, H>
where
    F: Future,
    H: IntoHeaderName + Clone,
{
    type Item = Service<F::Item, H>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        Ok(Service {
            authority: self.authority.clone(),
            canonical_dst_header: self.canonical_dst_header.clone(),
            inner,
        }
        .into())
    }
}

// === impl Service ===

impl<S, H, B> tower::Service<http::Request<B>> for Service<S, H>
where
    S: tower::Service<http::Request<B>>,
    H: IntoHeaderName + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        if let Some(new_authority) = self.authority.clone() {
            trace!(%new_authority, "Overwriting authority");
            h1::set_authority(req.uri_mut(), new_authority.clone());

            if let Ok(auth_val) = HeaderValue::from_str(new_authority.as_str()) {
                trace!(new_canonical_dst = %new_authority, "Overwriting canonical destination");
                if let Some(was_absolute) = req
                    .headers_mut()
                    .insert(self.canonical_dst_header.clone(), auth_val.clone())
                {
                    trace!(
                        "Removed original l5d-dst-canonical header {:?}",
                        was_absolute
                    );
                }

                if let Some(original_host) = req.headers_mut().insert(HOST, auth_val) {
                    trace!("Removed original host header {:?}", original_host);
                }
            }
        }
        self.inner.call(req)
    }
}

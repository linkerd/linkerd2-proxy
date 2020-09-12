use super::{h1, upgrade};
use futures::{future, ready, TryFuture, TryFutureExt};
use http;
use http::header::{HeaderValue, TRANSFER_ENCODING};
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tracing::{debug, trace, warn};

pub const L5D_ORIG_PROTO: &str = "l5d-orig-proto";

/// Upgrades HTTP requests from their original protocol to HTTP2.
#[derive(Clone, Debug)]
pub struct Upgrade<S> {
    inner: S,
}

/// Downgrades HTTP2 requests that were previousl upgraded to their original
/// protocol.
#[derive(Clone, Debug)]
pub struct Downgrade<S> {
    inner: S,
}

// ==== impl Upgrade =====

impl<S> Upgrade<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S, A, B> tower::Service<http::Request<A>> for Upgrade<S>
where
    S: tower::Service<http::Request<A>, Response = http::Response<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = UpgradeFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<A>) -> Self::Future {
        let version = req.version();
        if version != http::Version::HTTP_2
            && version != http::Version::HTTP_3
            && req.extensions().get::<upgrade::Http11Upgrade>().is_none()
        {
            let absolute_form = req
                .extensions_mut()
                .remove::<h1::WasAbsoluteForm>()
                .is_some();
            debug!(?version, absolute_form, "Upgrading request");

            // absolute-form is far less common, origin-form is the usual,
            // so only encode the extra information if it's different than
            // the normal.
            let val = match (version, absolute_form) {
                (http::Version::HTTP_11, false) => "HTTP/1.1",
                (http::Version::HTTP_11, true) => "HTTP/1.1; absolute-form",
                (http::Version::HTTP_10, false) => "HTTP/1.0",
                (http::Version::HTTP_10, true) => "HTTP/1.0; absolute-form",
                (v, _) => unreachable!("bad orig-proto version: {:?}", v),
            };
            req.headers_mut()
                .insert(L5D_ORIG_PROTO, HeaderValue::from_static(val));

            // transfer-encoding is illegal in HTTP2
            req.headers_mut().remove(TRANSFER_ENCODING);

            *req.version_mut() = http::Version::HTTP_2;
        } else {
            trace!(
                ?version,
                http.upgrade = req.extensions().get::<upgrade::Http11Upgrade>().is_some(),
                "Skipping upgrade",
            );
        }

        UpgradeFuture {
            version,
            inner: self.inner.call(req),
        }
    }
}

#[pin_project]
pub struct UpgradeFuture<F> {
    version: http::Version,
    #[pin]
    inner: F,
}

impl<F, B> Future for UpgradeFuture<F>
where
    F: TryFuture<Ok = http::Response<B>>,
{
    type Output = Result<F::Ok, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let mut res = ready!(this.inner.try_poll(cx))?;
        let version = res
            .headers_mut()
            .remove(L5D_ORIG_PROTO)
            .and_then(|orig_proto| {
                if orig_proto == "HTTP/1.1" {
                    Some(http::Version::HTTP_11)
                } else if orig_proto == "HTTP/1.0" {
                    Some(http::Version::HTTP_10)
                } else {
                    None
                }
            })
            .unwrap_or(*this.version);
        debug!("Downgrading response to {:?}", version);
        *res.version_mut() = version;
        Poll::Ready(Ok(res.into()))
    }
}

// ===== impl Downgrade =====

impl<S> Downgrade<S> {
    pub fn new<A, B>(inner: S) -> Self
    where
        S: tower::Service<http::Request<A>, Response = http::Response<B>>,
    {
        Self { inner }
    }
}

impl<S, A, B> tower::Service<http::Request<A>> for Downgrade<S>
where
    S: tower::Service<http::Request<A>, Response = http::Response<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = future::MapOk<S::Future, fn(S::Response) -> S::Response>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<A>) -> Self::Future {
        let mut upgrade_response = false;

        if req.version() == http::Version::HTTP_2 {
            if let Some(orig_proto) = req.headers_mut().remove(L5D_ORIG_PROTO) {
                debug!("translating HTTP2 to orig-proto: {:?}", orig_proto);

                let val: &[u8] = orig_proto.as_bytes();

                if val.starts_with(b"HTTP/1.1") {
                    *req.version_mut() = http::Version::HTTP_11;
                } else if val.starts_with(b"HTTP/1.0") {
                    *req.version_mut() = http::Version::HTTP_10;
                } else {
                    warn!("unknown {} header value: {:?}", L5D_ORIG_PROTO, orig_proto,);
                }

                if was_absolute_form(val) {
                    req.extensions_mut().insert(h1::WasAbsoluteForm(()));
                }
                upgrade_response = true;
            }
        }

        let fut = self.inner.call(req);

        if upgrade_response {
            fut.map_ok(|mut res| {
                let orig_proto = if res.version() == http::Version::HTTP_11 {
                    "HTTP/1.1"
                } else if res.version() == http::Version::HTTP_10 {
                    "HTTP/1.0"
                } else {
                    return res;
                };

                res.headers_mut()
                    .insert(L5D_ORIG_PROTO, HeaderValue::from_static(orig_proto));

                // transfer-encoding is illegal in HTTP2
                res.headers_mut().remove(TRANSFER_ENCODING);

                *res.version_mut() = http::Version::HTTP_2;
                res
            })
        } else {
            fut.map_ok(|res| res)
        }
    }
}

fn was_absolute_form(val: &[u8]) -> bool {
    val.len() >= "HTTP/1.1; absolute-form".len() && &val[10..23] == b"absolute-form"
}

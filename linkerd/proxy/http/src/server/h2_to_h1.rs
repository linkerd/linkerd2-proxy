use super::{UriWasOriginallyAbsoluteForm, WasHttp1OverH2};
use crate::header::L5D_ORIG_PROTO;
use futures::prelude::*;
use http::header::{HeaderValue, TRANSFER_ENCODING};
use linkerd_error::Result;
use linkerd_stack::Service;
use std::task::{Context, Poll};
use tracing::{debug, warn};

/// Downgrades HTTP2 requests that were previousl upgraded to their original
/// protocol.
#[derive(Clone, Debug)]
pub struct Downgrade<S> {
    inner: S,
    enabled: bool,
}

// === impl Downgrade ===

impl<S> Downgrade<S> {
    pub(super) fn new(enabled: bool, inner: S) -> Self {
        Self { enabled, inner }
    }
}

type DowngradeFuture<F, T> = future::MapOk<F, fn(T) -> T>;

impl<S, A, B> Service<http::Request<A>> for Downgrade<S>
where
    S: Service<http::Request<A>, Response = http::Response<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = DowngradeFuture<S::Future, S::Response>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<A>) -> Self::Future {
        let mut upgrade_response = false;

        if self.enabled && req.version() == http::Version::HTTP_2 {
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
                    req.extensions_mut()
                        .insert(UriWasOriginallyAbsoluteForm(()));
                }
                req.extensions_mut().insert(WasHttp1OverH2(()));
                upgrade_response = true;
            }
        }

        let fut = self.inner.call(req);

        if upgrade_response {
            fut.map_ok(|mut res| {
                let orig_proto = match res.version() {
                    http::Version::HTTP_11 => "HTTP/1.1",
                    http::Version::HTTP_10 => "HTTP/1.0",
                    _ => return res,
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

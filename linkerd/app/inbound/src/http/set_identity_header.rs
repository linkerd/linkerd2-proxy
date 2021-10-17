use linkerd_app_core::{proxy::http, svc, tls};
use std::task::{Context, Poll};
use tracing::{debug, trace};

const HEADER_NAME: &str = "l5d-client-id";

#[derive(Clone, Debug)]
pub struct NewSetIdentityHeader<P, N> {
    params: P,
    inner: N,
}

#[derive(Clone, Debug)]
pub struct SetIdentityHeader<M> {
    inner: M,
    value: Option<http::HeaderValue>,
}

// === impl NewSetIdentityHeader ===

impl<P: Clone, N> NewSetIdentityHeader<P, N> {
    pub fn layer(params: P) -> impl svc::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self {
            inner,
            params: params.clone(),
        })
    }
}

impl<T, P, N> svc::NewService<T> for NewSetIdentityHeader<P, N>
where
    P: svc::ExtractParam<tls::ConditionalServerTls, T>,
    N: svc::NewService<T>,
{
    type Service = SetIdentityHeader<N::Service>;

    #[inline]
    fn new_service(&self, t: T) -> Self::Service {
        let value = self
            .params
            .extract_param(&t)
            .value()
            .and_then(|tls| match tls {
                tls::ServerTls::Established { client_id, .. } => {
                    client_id.as_ref().and_then(|id| {
                        match http::HeaderValue::from_str(id.as_str()) {
                            Ok(v) => Some(v),
                            Err(error) => {
                                tracing::warn!(%error, "identity not a valid header value");
                                None
                            }
                        }
                    })
                }
                _ => None,
            });
        SetIdentityHeader {
            value,
            inner: self.inner.new_service(t),
        }
    }
}

// === impl Service ===

impl<S, B> tower::Service<http::Request<B>> for SetIdentityHeader<S>
where
    S: tower::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        let prior = if let Some(id) = self.value.clone() {
            trace!(header = %HEADER_NAME, ?id, "Setting identity header");
            req.headers_mut().insert(HEADER_NAME, id)
        } else {
            req.headers_mut().remove(HEADER_NAME)
        };
        if let Some(value) = prior {
            debug!(header = %HEADER_NAME, ?value, "Stripped identity header");
        }

        self.inner.call(req)
    }
}

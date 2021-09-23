use linkerd_app_core::{
    errors::respond::L5D_PROXY_ERROR,
    proxy::http,
    svc::{self, NewService},
};

#[derive(Clone, Debug)]
pub struct NewStripProxyError<N> {
    strip: bool,
    inner: N,
}

impl<N> NewStripProxyError<N> {
    pub fn layer(emit_headers: bool) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self {
            strip: !emit_headers,
            inner,
        })
    }
}

impl<T, N> NewService<T> for NewStripProxyError<N>
where
    N: NewService<T>,
{
    type Service = svc::Either<
        N::Service,
        http::strip_header::response::StripHeader<&'static str, N::Service>,
    >;

    fn new_service(&self, target: T) -> Self::Service {
        let inner = self.inner.new_service(target);

        if self.strip {
            return svc::Either::B(http::StripHeader::response(L5D_PROXY_ERROR, inner));
        };

        svc::Either::A(inner)
    }
}

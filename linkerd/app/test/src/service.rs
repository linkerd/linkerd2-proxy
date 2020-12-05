use crate::{
    app_core::{proxy::http, svc},
    Error,
};

/// Returns a mock HTTP router that asserts that the HTTP router is never used.
pub fn no_http() -> NoHttp {
    NoHttp
}

#[derive(Clone)]
pub struct NoHttp;

impl<T: std::fmt::Debug> svc::NewService<T> for NoHttp {
    type Service = Self;
    fn new_service(&mut self, target: T) -> Self::Service {
        panic!("the HTTP router should not be used in this test, but we tried to build a service for {:?}", target)
    }
}

impl svc::Service<http::Request<http::boxed::BoxBody>> for NoHttp {
    type Response = http::Response<http::boxed::BoxBody>;
    type Error = Error;
    type Future = futures::future::Ready<Result<Self::Response, Self::Error>>;
    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        panic!("http services should not be used in this test!")
    }

    fn call(&mut self, _: http::Request<http::boxed::BoxBody>) -> Self::Future {
        panic!("http services should not be used in this test!")
    }
}

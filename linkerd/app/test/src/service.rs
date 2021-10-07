use crate::{
    app_core::{proxy::http, svc},
    Error,
};

/// Returns a mock HTTP router that asserts that the HTTP router is never used.
pub fn no_http<T>() -> NoHttp<T> {
    NoHttp(std::marker::PhantomData)
}

#[derive(Clone, Copy, Debug)]
pub struct NoHttp<T>(std::marker::PhantomData<fn(T)>);

impl<T: std::fmt::Debug> svc::NewService<T> for NoHttp<T> {
    type Service = Self;
    fn new_service(&self, target: T) -> Self::Service {
        panic!("the HTTP router should not be used in this test, but we tried to build a service for {:?}", target)
    }
}

impl<T> svc::Service<http::Request<http::BoxBody>> for NoHttp<T> {
    type Response = http::Response<http::BoxBody>;
    type Error = Error;
    type Future = futures::future::Ready<Result<Self::Response, Self::Error>>;
    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        panic!("http services should not be used in this test!")
    }

    fn call(&mut self, _: http::Request<http::BoxBody>) -> Self::Future {
        panic!("http services should not be used in this test!")
    }
}

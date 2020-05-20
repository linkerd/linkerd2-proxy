// #![deny(warnings, rust_2018_idioms)]

// use linkerd2_error::Error;
// use linkerd2_proxy_core::Resolve;
// use std::fmt;
// use std::time::Duration;

pub mod buffer;
// pub mod from_resolve;
// pub mod make_endpoint;

// use self::buffer::Buffer;
// use self::from_resolve::FromResolve;
// use self::make_endpoint::MakeEndpoint;

// #[derive(Clone, Debug)]
// pub struct Layer<T, R> {
//     capacity: usize,
//     watchdog: Duration,
//     resolve: R,
//     _marker: std::marker::PhantomData<fn(T)>,
// }

// // === impl Layer ===

// impl<T, R> Layer<T, R> {
//     pub fn new(capacity: usize, watchdog: Duration, resolve: R) -> Self
//     where
//         R: Resolve<T> + Clone,
//         R::Endpoint: fmt::Debug + Clone + PartialEq,
//     {
//         Self {
//             capacity,
//             watchdog,
//             resolve,
//             _marker: std::marker::PhantomData,
//         }
//     }
// }

// impl<T, R, M> tower::layer::Layer<M> for Layer<T, R>
// where
//     T: fmt::Display,
//     R: Resolve<T> + Send + Clone + 'static,
//     R::Error: Into<Error>,
//     R::Endpoint: fmt::Debug + Clone + PartialEq + Send,
//     R::Resolution: Send + 'static,
//     R::Future: Send + 'static,
//     M: tower::Service<R::Endpoint> + Clone + Send + 'static,
//     M::Error: Into<Error>,
//     M::Response: Send + 'static,
//     M::Future: Send + 'static,
// {
//     type Service = Buffer<MakeEndpoint<FromResolve<R>, M>>;

//     fn layer(&self, make_endpoint: M) -> Self::Service {
//         let make_discover =
//             MakeEndpoint::new(make_endpoint, FromResolve::new(self.resolve.clone()));
//         Buffer::new(self.capacity, self.watchdog, make_discover)
//     }
// }

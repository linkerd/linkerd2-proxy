// #![deny(warnings, rust_2018_idioms)]

// use futures::{Future, Poll};
// use linkerd2_error::Error;
// use linkerd2_stack::Proxy;
// use std::time::Duration;
// use tokio_connect::Connect;
// use tokio_timer as timer;

// pub mod error;
// mod failfast;
// mod idle;
// mod probe_ready;

// pub use self::{
//     failfast::{FailFast, FailFastError, FailFastLayer},
//     idle::{Idle, IdleError, IdleLayer},
//     probe_ready::{ProbeReady, ProbeReadyLayer},
// };

// /// A timeout that wraps an underlying operation.
// #[derive(Debug, Clone)]
// pub struct Timeout<T> {
//     inner: T,
//     duration: Option<Duration>,
// }

// pub enum TimeoutFuture<F> {
//     Passthru(F),
//     Timeout(timer::Timeout<F>, Duration),
// }

// //===== impl Timeout =====

// impl<T> Timeout<T> {
//     /// Construct a new `Timeout` wrapping `inner`.
//     pub fn new(inner: T, duration: Duration) -> Self {
//         Timeout {
//             inner,
//             duration: Some(duration),
//         }
//     }

//     pub fn passthru(inner: T) -> Self {
//         Timeout {
//             inner,
//             duration: None,
//         }
//     }
// }

// impl<P, S, Req> Proxy<Req, S> for Timeout<P>
// where
//     P: Proxy<Req, S>,
//     S: tower::Service<P::Request>,
// {
//     type Request = P::Request;
//     type Response = P::Response;
//     type Error = Error;
//     type Future = TimeoutFuture<P::Future>;

//     fn proxy(&self, svc: &mut S, req: Req) -> Self::Future {
//         let inner = self.inner.proxy(svc, req);
//         match self.duration {
//             None => TimeoutFuture::Passthru(inner),
//             Some(t) => TimeoutFuture::Timeout(timer::Timeout::new(inner, t), t),
//         }
//     }
// }

// impl<S, Req> tower::Service<Req> for Timeout<S>
// where
//     S: tower::Service<Req>,
//     S::Error: Into<Error>,
// {
//     type Response = S::Response;
//     type Error = Error;
//     type Future = TimeoutFuture<S::Future>;

//     fn poll_ready(&mut self) -> Poll<(), Self::Error> {
//         self.inner.poll_ready().map_err(Into::into)
//     }

//     fn call(&mut self, req: Req) -> Self::Future {
//         let inner = self.inner.call(req);
//         match self.duration {
//             None => TimeoutFuture::Passthru(inner),
//             Some(t) => TimeoutFuture::Timeout(timer::Timeout::new(inner, t), t),
//         }
//     }
// }

// impl<C> Connect for Timeout<C>
// where
//     C: Connect,
//     C::Error: Into<Error>,
// {
//     type Connected = C::Connected;
//     type Error = Error;
//     type Future = TimeoutFuture<C::Future>;

//     fn connect(&self) -> Self::Future {
//         let inner = self.inner.connect();
//         match self.duration {
//             None => TimeoutFuture::Passthru(inner),
//             Some(t) => TimeoutFuture::Timeout(timer::Timeout::new(inner, t), t),
//         }
//     }
// }

// impl<F> Future for TimeoutFuture<F>
// where
//     F: Future,
//     F::Error: Into<Error>,
// {
//     type Item = F::Item;
//     type Error = Error;

//     fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
//         match self {
//             TimeoutFuture::Passthru(f) => f.poll().map_err(Into::into),
//             TimeoutFuture::Timeout(f, duration) => f.poll().map_err(|error| {
//                 if error.is_timer() {
//                     return error
//                         .into_timer()
//                         .expect("error.into_timer() must succeed if error.is_timer()")
//                         .into();
//                 }

//                 if error.is_elapsed() {
//                     return error::ResponseTimeout(*duration).into();
//                 }

//                 error
//                     .into_inner()
//                     .expect("if error is not elapsed or timer, must be inner")
//                     .into()
//             }),
//         }
//     }
// }

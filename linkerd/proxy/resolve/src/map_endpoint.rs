// //! A middleware that wraps `Resolutions`, modifying their endpoint type.

// use futures::{try_ready, Async, Future, Poll};
// use linkerd2_proxy_core::resolve;
// use std::net::SocketAddr;

// pub trait MapEndpoint<Target, In> {
//     type Out;
//     fn map_endpoint(&self, target: &Target, addr: SocketAddr, in_ep: In) -> Self::Out;
// }

// #[derive(Clone, Debug)]
// pub struct Resolve<M, R> {
//     resolve: R,
//     map: M,
// }

// #[derive(Debug)]
// pub struct ResolveFuture<T, F, M> {
//     future: F,
//     target: Option<T>,
//     map: Option<M>,
// }

// #[derive(Clone, Debug)]
// pub struct Resolution<T, M, R> {
//     resolution: R,
//     target: T,
//     map: M,
// }

// // === impl Resolve ===

// impl<M, R> Resolve<M, R> {
//     pub fn new<T>(map: M, resolve: R) -> Self
//     where
//         Self: resolve::Resolve<T>,
//     {
//         Self { resolve, map }
//     }
// }

// impl<T, M, R> tower::Service<T> for Resolve<M, R>
// where
//     T: Clone,
//     R: resolve::Resolve<T>,
//     M: MapEndpoint<T, R::Endpoint> + Clone,
// {
//     type Response = Resolution<T, M, R::Resolution>;
//     type Error = R::Error;
//     type Future = ResolveFuture<T, R::Future, M>;

//     #[inline]
//     fn poll_ready(&mut self) -> Poll<(), Self::Error> {
//         self.resolve.poll_ready()
//     }

//     #[inline]
//     fn call(&mut self, target: T) -> Self::Future {
//         let future = self.resolve.resolve(target.clone());
//         Self::Future {
//             future,
//             target: Some(target),
//             map: Some(self.map.clone()),
//         }
//     }
// }

// // === impl ResolveFuture ===

// impl<T, F, M> Future for ResolveFuture<T, F, M>
// where
//     F: Future,
//     F::Item: resolve::Resolution,
//     M: MapEndpoint<T, <F::Item as resolve::Resolution>::Endpoint>,
// {
//     type Item = Resolution<T, M, F::Item>;
//     type Error = F::Error;

//     fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
//         let resolution = try_ready!(self.future.poll());
//         let target = self.target.take().expect("polled after ready");
//         let map = self.map.take().expect("polled after ready");
//         Ok(Async::Ready(Resolution {
//             resolution,
//             target,
//             map,
//         }))
//     }
// }

// // === impl Resolution ===

// impl<T, M, R> resolve::Resolution for Resolution<T, M, R>
// where
//     R: resolve::Resolution,
//     M: MapEndpoint<T, R::Endpoint>,
// {
//     type Endpoint = M::Out;
//     type Error = R::Error;

//     fn poll(&mut self) -> Poll<resolve::Update<M::Out>, Self::Error> {
//         let update = match try_ready!(self.resolution.poll()) {
//             resolve::Update::Add(eps) => resolve::Update::Add(
//                 eps.into_iter()
//                     .map(|(a, ep)| {
//                         let ep = self.map.map_endpoint(&self.target, a, ep);
//                         (a, ep)
//                     })
//                     .collect(),
//             ),
//             resolve::Update::Remove(addrs) => resolve::Update::Remove(addrs),
//             resolve::Update::DoesNotExist => resolve::Update::DoesNotExist,
//             resolve::Update::Empty => resolve::Update::Empty,
//         };
//         Ok(update.into())
//     }
// }

// // === impl MapEndpoint ===

// impl<T, N> MapEndpoint<T, N> for () {
//     type Out = N;

//     fn map_endpoint(&self, _: &T, _: SocketAddr, ep: N) -> Self::Out {
//         ep
//     }
// }

// impl<T, In, Out, F: Fn(&T, SocketAddr, In) -> Out> MapEndpoint<T, In> for F {
//     type Out = Out;

//     fn map_endpoint(&self, target: &T, addr: SocketAddr, ep: In) -> Self::Out {
//         (self)(target, addr, ep)
//     }
// }

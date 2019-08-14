pub mod req_body_as_payload {
    use super::super::GrpcBody;
    use crate::svc;
    use futures::Poll;
    use http;
    use hyper::body::Payload;

    #[derive(Debug)]
    pub struct Service<S>(S);

    pub fn layer<S, B>() -> impl svc::Layer<S, Service = Service<S>> + Copy
    where
        GrpcBody<B>: Payload,
        S: svc::Service<http::Request<GrpcBody<B>>>,
    {
        svc::layer::mk(Service)
    }

    // === impl Service ===

    impl<B, S> svc::Service<http::Request<B>> for Service<S>
    where
        GrpcBody<B>: Payload,
        S: svc::Service<http::Request<GrpcBody<B>>>,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = S::Future;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.0.poll_ready()
        }

        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            self.0.call(req.map(GrpcBody::new))
        }
    }
}

pub mod req_box_body {
    use crate::svc;
    use bytes::Bytes;
    use futures::Poll;
    use http;
    use tower_grpc::{Body, BoxBody};

    pub struct Service<S>(S);

    impl<S> Service<S> {
        pub fn new(service: S) -> Self {
            Service(service)
        }
    }

    impl<B, S> svc::Service<http::Request<B>> for Service<S>
    where
        B: Body + Send + 'static,
        Bytes: From<B::Data>,
        S: svc::Service<http::Request<BoxBody>>,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = S::Future;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.0.poll_ready()
        }

        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            self.0.call(req.map(BoxBody::map_from))
        }
    }
}

pub mod res_body_as_payload {
    use super::super::GrpcBody;
    use crate::svc;
    use futures::{future, Future, Poll};
    use http;
    use hyper::body::Payload;
    use tower_grpc::Body;

    pub struct Service<S>(S);

    impl<S> Service<S> {
        pub fn new(service: S) -> Self {
            Service(service)
        }
    }

    impl<B1, B2, S> svc::Service<http::Request<B1>> for Service<S>
    where
        B2: Body,
        GrpcBody<B2>: Payload,
        S: svc::Service<http::Request<B1>, Response = http::Response<B2>>,
    {
        type Response = http::Response<GrpcBody<B2>>;
        type Error = S::Error;
        type Future =
            future::Map<S::Future, fn(http::Response<B2>) -> http::Response<GrpcBody<B2>>>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.0.poll_ready()
        }

        fn call(&mut self, req: http::Request<B1>) -> Self::Future {
            self.0.call(req).map(|res| res.map(GrpcBody::new))
        }
    }
}

pub mod unauthenticated {
    use crate::api::tap as api;
    use futures::{future, stream};
    use tower_grpc::{Code, Request, Response, Status};

    #[derive(Clone)]
    pub struct Unauthenticated;

    impl api::server::Tap for Unauthenticated {
        type ObserveStream = stream::Empty<api::TapEvent, Status>;
        type ObserveFuture = future::FutureResult<Response<Self::ObserveStream>, Status>;

        fn observe(&mut self, _req: Request<api::ObserveRequest>) -> Self::ObserveFuture {
            future::err(Status::new(
                Code::Unauthenticated,
                "client is not authenticated for the tap server",
            ))
        }
    }
}

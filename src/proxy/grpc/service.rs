pub mod req_body_as_payload {
    use futures::Poll;
    use http;
    use hyper::body::Payload;

    use super::super::GrpcBody;
    use svc;

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
    use bytes::Bytes;
    use futures::Poll;
    use http;
    use tower_grpc::{Body, BoxBody};

    use svc;

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
    use futures::{future, Future, Poll};
    use http;
    use hyper::body::Payload;
    use tower_grpc::Body;

    use super::super::GrpcBody;
    use svc;

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

pub mod unauthorized {
    use futures::{
        future::{self, FutureResult},
        Poll,
    };
    use http::{self, StatusCode};
    use hyper::{Body, Response};
    use std::io;
    use svc;

    pub struct Service;

    impl Service {
        pub fn new() -> Self {
            Service
        }
    }

    impl<B> svc::Service<http::Request<B>> for Service {
        type Response = http::Response<Body>;
        type Error = io::Error;
        type Future = FutureResult<http::Response<Body>, Self::Error>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            Ok(().into())
        }

        fn call(&mut self, _req: http::Request<B>) -> Self::Future {
            let rsp = Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::empty())
                .expect("builder with known status code should not fail");
            return future::ok(rsp);
        }
    }
}

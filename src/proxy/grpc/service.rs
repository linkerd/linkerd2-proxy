pub mod req_body_as_payload {
    use bytes::Bytes;
    use futures::Poll;
    use http;
    use tower_grpc::Body;

    use super::super::GrpcBody;
    use svc;

    #[derive(Clone, Debug)]
    pub struct Layer;

    #[derive(Clone, Debug)]
    pub struct Stack<M> {
        inner: M,
    }

    #[derive(Debug)]
    pub struct Service<S>(S);

    // === impl Layer ===

    pub fn layer() -> Layer {
        Layer
    }

    impl<T, M> svc::Layer<T, T, M> for Layer
    where
        M: svc::Stack<T>,
    {
        type Value = <Stack<M> as svc::Stack<T>>::Value;
        type Error = <Stack<M> as svc::Stack<T>>::Error;
        type Stack = Stack<M>;

        fn bind(&self, inner: M) -> Self::Stack {
            Stack { inner }
        }
    }

    // === impl Stack ===

    impl<T, M> svc::Stack<T> for Stack<M>
    where
        M: svc::Stack<T>,
    {
        type Value = Service<M::Value>;
        type Error = M::Error;

        fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
            let inner = self.inner.make(target)?;
            Ok(Service(inner))
        }
    }

    // === impl Service ===

    impl<B, S> svc::Service<http::Request<B>> for Service<S>
    where
        B: Body<Data = Bytes> + Send + 'static,
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
        B: Body<Data = Bytes> + Send + 'static,
        S: svc::Service<http::Request<BoxBody>>,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = S::Future;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.0.poll_ready()
        }

        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            self.0.call(req.map(|b| BoxBody::new(Box::new(b))))
        }
    }
}

pub mod res_body_as_payload {
    use futures::{future, Future, Poll};
    use http;
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

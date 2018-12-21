macro_rules! gen_layer {
    ($Service:ident) => (
        #[derive(Clone, Debug)]
        pub struct Layer;

        #[derive(Clone, Debug)]
        pub struct Stack<M> {
            inner: M,
        }

        // === impl Layer ===

        pub fn layer() -> Layer {
            Layer
        }

        impl<Target, M> svc::Layer<Target, Target, M> for Layer
        where
            M: svc::Stack<Target>,
        {
            type Value = <Stack<M> as svc::Stack<Target>>::Value;
            type Error = <Stack<M> as svc::Stack<Target>>::Error;
            type Stack = Stack<M>;

            fn bind(&self, inner: M) -> Self::Stack {
                Stack { inner }
            }
        }

        // === impl Stack ===

        impl<Target, M> svc::Stack<Target> for Stack<M>
        where
            M: svc::Stack<Target>,
        {
            type Value = $Service<M::Value>;
            type Error = M::Error;

            fn make(&self, target: &Target) -> Result<Self::Value, Self::Error> {
                let inner = self.inner.make(target)?;
                Ok($Service(inner))
            }
        }
    );
}

pub mod req_body_as_payload {
    use bytes::Bytes;
    use http;
    use futures::Poll;
    use tower_grpc::Body;

    use super::super::GrpcBody;
    use svc;

    #[derive(Clone, Debug)]
    pub struct Service<S>(S);

    gen_layer!(Service);

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

pub mod server {
    use bytes::Bytes;
    use http;
    use futures::{future, Future, Poll};
    use tower_grpc::{Body, BoxBody};

    use super::super::GrpcBody;
    use svc;

    #[derive(Clone, Debug)]
    pub struct Service<S>(S);

    gen_layer!(Service);

    // === impl Service ===

    impl<B1, B2, S> svc::Service<http::Request<B1>> for Service<S>
    where
        B1: Body + Send + 'static,
        B1::Data: Into<Bytes>,
        B2: Body,
        S: svc::Service<
            http::Request<BoxBody>,
            Response = http::Response<B2>,
        >,
    {
        type Response = http::Response<GrpcBody<B2>>;
        type Error = S::Error;
        type Future = future::Map<S::Future, fn(http::Response<B2>) -> http::Response<GrpcBody<B2>>>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.0.poll_ready()
        }

        fn call(&mut self, req: http::Request<B1>) -> Self::Future {
            let req = req.map(|b| BoxBody::new(Box::new(GrpcBody::new(b))));
            self.0.call(req)
                .map(|res| {
                    res.map(GrpcBody::new)
                })
        }
    }
}


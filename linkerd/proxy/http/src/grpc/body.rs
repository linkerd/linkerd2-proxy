use futures::Poll;
use http;
use hyper::body::Payload;
use tower_grpc as grpc;

#[derive(Debug)]
pub struct GrpcBody<B>(B);

// ===== impl GrpcBody =====

impl<B> GrpcBody<B> {
    pub fn new(inner: B) -> Self {
        GrpcBody(inner)
    }
}

impl<B> Payload for GrpcBody<B>
where
    B: grpc::Body + Send + 'static,
    B::Data: Send + 'static,
{
    type Data = B::Data;
    type Error = B::Error;

    fn is_end_stream(&self) -> bool {
        grpc::Body::is_end_stream(&self.0)
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        grpc::Body::poll_data(&mut self.0)
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        grpc::Body::poll_trailers(&mut self.0)
    }
}

impl<B> http_body::Body for GrpcBody<B>
where
    B: grpc::Body,
{
    type Data = B::Data;
    type Error = B::Error;

    fn is_end_stream(&self) -> bool {
        grpc::Body::is_end_stream(&self.0)
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        grpc::Body::poll_data(&mut self.0)
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        grpc::Body::poll_trailers(&mut self.0)
    }
}

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
    B::Item: Send + 'static,
{
    type Data = B::Item;
    type Error = B::Error;

    fn is_end_stream(&self) -> bool {
        grpc::Body::is_end_stream(&self.0)
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        grpc::Body::poll_buf(&mut self.0)
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        grpc::Body::poll_trailers(&mut self.0)
    }
}

impl<B> tower_http_service::Body for GrpcBody<B>
where
    B: grpc::Body,
{
    type Item = B::Item;
    type Error = B::Error;

    fn is_end_stream(&self) -> bool {
        grpc::Body::is_end_stream(&self.0)
    }

    fn poll_buf(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        grpc::Body::poll_buf(&mut self.0)
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        grpc::Body::poll_trailers(&mut self.0)
    }
}

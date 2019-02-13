use bytes::IntoBuf;
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
    <B::Data as IntoBuf>::Buf: Send + 'static,
{
    type Data = <B::Data as IntoBuf>::Buf;
    type Error = h2::Error;

    fn is_end_stream(&self) -> bool {
        grpc::Body::is_end_stream(&self.0)
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        let data = try_ready!(grpc::Body::poll_data(&mut self.0));
        Ok(data.map(IntoBuf::into_buf).into())
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        grpc::Body::poll_metadata(&mut self.0).map_err(From::from)
    }
}

impl<B> grpc::Body for GrpcBody<B>
where
    B: grpc::Body,
{
    type Data = B::Data;

    fn is_end_stream(&self) -> bool {
        grpc::Body::is_end_stream(&self.0)
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, grpc::Error> {
        grpc::Body::poll_data(&mut self.0)
    }

    fn poll_metadata(&mut self) -> Poll<Option<http::HeaderMap>, grpc::Error> {
        grpc::Body::poll_metadata(&mut self.0)
    }
}

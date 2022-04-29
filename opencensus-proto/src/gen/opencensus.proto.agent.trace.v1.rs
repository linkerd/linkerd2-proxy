#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CurrentLibraryConfig {
    /// This is required only in the first message on the stream or if the
    /// previous sent CurrentLibraryConfig message has a different Node (e.g.
    /// when the same RPC is used to configure multiple Applications).
    #[prost(message, optional, tag="1")]
    pub node: ::core::option::Option<super::super::common::v1::Node>,
    /// Current configuration.
    #[prost(message, optional, tag="2")]
    pub config: ::core::option::Option<super::super::super::trace::v1::TraceConfig>,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct UpdatedLibraryConfig {
    /// This field is ignored when the RPC is used to configure only one Application.
    /// This is required only in the first message on the stream or if the
    /// previous sent UpdatedLibraryConfig message has a different Node.
    #[prost(message, optional, tag="1")]
    pub node: ::core::option::Option<super::super::common::v1::Node>,
    /// Requested updated configuration.
    #[prost(message, optional, tag="2")]
    pub config: ::core::option::Option<super::super::super::trace::v1::TraceConfig>,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ExportTraceServiceRequest {
    /// This is required only in the first message on the stream or if the
    /// previous sent ExportTraceServiceRequest message has a different Node (e.g.
    /// when the same RPC is used to send Spans from multiple Applications).
    #[prost(message, optional, tag="1")]
    pub node: ::core::option::Option<super::super::common::v1::Node>,
    /// A list of Spans that belong to the last received Node.
    #[prost(message, repeated, tag="2")]
    pub spans: ::prost::alloc::vec::Vec<super::super::super::trace::v1::Span>,
    /// The resource for the spans in this message that do not have an explicit
    /// resource set.
    /// If unset, the most recently set resource in the RPC stream applies. It is
    /// valid to never be set within a stream, e.g. when no resource info is known.
    #[prost(message, optional, tag="3")]
    pub resource: ::core::option::Option<super::super::super::resource::v1::Resource>,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ExportTraceServiceResponse {
}
/// Generated client implementations.
pub mod trace_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    /// Service that can be used to push spans and configs between one Application
    /// instrumented with OpenCensus and an agent, or between an agent and a
    /// central collector or config service (in this case spans and configs are
    /// sent/received to/from multiple Applications).
    #[derive(Debug, Clone)]
    pub struct TraceServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl<T> TraceServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
    {
        pub fn new(inner: T) -> Self {
            let inner = tonic::client::Grpc::new(inner);
            Self { inner }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> TraceServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            TraceServiceClient::new(InterceptedService::new(inner, interceptor))
        }
        /// Compress requests with `gzip`.
        ///
        /// This requires the server to support it otherwise it might respond with an
        /// error.
        #[must_use]
        pub fn send_gzip(mut self) -> Self {
            self.inner = self.inner.send_gzip();
            self
        }
        /// Enable decompressing responses with `gzip`.
        #[must_use]
        pub fn accept_gzip(mut self) -> Self {
            self.inner = self.inner.accept_gzip();
            self
        }
        /// After initialization, this RPC must be kept alive for the entire life of
        /// the application. The agent pushes configs down to applications via a
        /// stream.
        pub async fn config(
            &mut self,
            request: impl tonic::IntoStreamingRequest<
                Message = super::CurrentLibraryConfig,
            >,
        ) -> Result<
                tonic::Response<tonic::codec::Streaming<super::UpdatedLibraryConfig>>,
                tonic::Status,
            > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/opencensus.proto.agent.trace.v1.TraceService/Config",
            );
            self.inner.streaming(request.into_streaming_request(), path, codec).await
        }
        /// For performance reasons, it is recommended to keep this RPC
        /// alive for the entire life of the application.
        pub async fn export(
            &mut self,
            request: impl tonic::IntoStreamingRequest<
                Message = super::ExportTraceServiceRequest,
            >,
        ) -> Result<
                tonic::Response<
                    tonic::codec::Streaming<super::ExportTraceServiceResponse>,
                >,
                tonic::Status,
            > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/opencensus.proto.agent.trace.v1.TraceService/Export",
            );
            self.inner.streaming(request.into_streaming_request(), path, codec).await
        }
    }
}

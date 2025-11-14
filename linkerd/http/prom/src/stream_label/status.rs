//! [`MkStreamLabel`] and [`StreamLabel`] implementations for status codes.
//!
//! This submodule provides [`MkLabelGrpcStatus`] and [`MkLabelHttpStatus`]

use crate::stream_label::{MkStreamLabel, StreamLabel};
use http::{HeaderMap, HeaderValue, Response, StatusCode};
use linkerd_error::Error;
use tonic::Code;

/// A [`MkStreamLabel`] implementation for gRPC traffic.
///
/// This generates [`LabelGrpcStatus`] labelers.
#[derive(Clone, Debug)]
pub struct MkLabelGrpcStatus;

/// A [`StreamLabel`] implementation for gRPC traffic.
///
/// This will inspect response headers and trailers for a [`Code`].
#[derive(Clone, Debug, Default)]
pub struct LabelGrpcStatus {
    code: Option<Code>,
}

/// A [`MkStreamLabel`] implementation for HTTP traffic.
///
/// This generates [`LabelHttpStatus`] labelers.
#[derive(Clone, Debug)]
pub struct MkLabelHttpStatus;

/// A [`StreamLabel`] implementation for HTTP traffic.
///
/// This will inspect the response headers for a [`StatusCode`].
#[derive(Clone, Debug, Default)]
pub struct LabelHttpStatus {
    status: Option<StatusCode>,
}

// === impl MkLabelGrpcStatus ===

impl MkStreamLabel for MkLabelGrpcStatus {
    type DurationLabels = <Self::StreamLabel as StreamLabel>::DurationLabels;
    type StatusLabels = <Self::StreamLabel as StreamLabel>::StatusLabels;
    type StreamLabel = LabelGrpcStatus;

    fn mk_stream_labeler<B>(&self, _: &http::Request<B>) -> Option<Self::StreamLabel> {
        Some(LabelGrpcStatus::default())
    }
}

// === LabelGrpcStatus ===

impl StreamLabel for LabelGrpcStatus {
    type DurationLabels = ();
    type StatusLabels = Option<Code>;

    fn init_response<B>(&mut self, rsp: &Response<B>) {
        let headers = rsp.headers();
        self.code = Self::get_grpc_status(headers);
    }

    fn end_response(&mut self, trailers: Result<Option<&HeaderMap>, &Error>) {
        let Ok(Some(trailers)) = trailers else { return };
        self.code = Self::get_grpc_status(trailers);
    }

    fn status_labels(&self) -> Self::StatusLabels {
        self.code
    }

    fn duration_labels(&self) -> Self::DurationLabels {}
}

impl LabelGrpcStatus {
    fn get_grpc_status(headers: &HeaderMap) -> Option<Code> {
        headers
            .get("grpc-status")
            .map(HeaderValue::as_bytes)
            .map(tonic::Code::from_bytes)
    }
}

// === impl MkLabelHttpStatus ===

impl MkStreamLabel for MkLabelHttpStatus {
    type DurationLabels = <Self::StreamLabel as StreamLabel>::DurationLabels;
    type StatusLabels = <Self::StreamLabel as StreamLabel>::StatusLabels;
    type StreamLabel = LabelHttpStatus;

    fn mk_stream_labeler<B>(&self, _: &http::Request<B>) -> Option<Self::StreamLabel> {
        Some(LabelHttpStatus::default())
    }
}

// === impl LabelHttpStatus ===

impl StreamLabel for LabelHttpStatus {
    type DurationLabels = ();
    type StatusLabels = Option<StatusCode>;

    fn init_response<B>(&mut self, rsp: &http::Response<B>) {
        self.status = Some(rsp.status());
    }

    fn end_response(&mut self, _: Result<Option<&http::HeaderMap>, &linkerd_error::Error>) {}

    fn status_labels(&self) -> Self::StatusLabels {
        self.status
    }

    fn duration_labels(&self) -> Self::DurationLabels {}
}

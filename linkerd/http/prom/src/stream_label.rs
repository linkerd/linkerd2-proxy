//! Stream labeling facilities.

use linkerd_error::Error;
use prometheus_client::encoding::EncodeLabelSet;

pub mod status;

/// A strategy for labeling request/responses streams for status and duration
/// metrics.
///
/// This is specifically to support higher-cardinality status counters and
/// lower-cardinality stream duration histograms.
pub trait MkStreamLabel {
    type DurationLabels;
    type StatusLabels;

    type StreamLabel: StreamLabel<
        DurationLabels = Self::DurationLabels,
        StatusLabels = Self::StatusLabels,
    >;

    /// Returns None when the request should not be recorded.
    fn mk_stream_labeler<B>(&self, req: &http::Request<B>) -> Option<Self::StreamLabel>;
}

pub trait StreamLabel: Send + 'static {
    type DurationLabels;
    type StatusLabels;

    fn init_response<B>(&mut self, rsp: &http::Response<B>);
    fn end_response(&mut self, trailers: Result<Option<&http::HeaderMap>, &Error>);

    fn status_labels(&self) -> Self::StatusLabels;
    fn duration_labels(&self) -> Self::DurationLabels;
}

/// A label set.
///
/// This trait is a shorthand for common trait bounds that are required of Prometheus label sets.
pub trait LabelSet:
    EncodeLabelSet + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static
{
}

impl<T> LabelSet for T where
    T: EncodeLabelSet + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static
{
}

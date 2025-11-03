//! [`StreamLabel`] implementation for labels known in advance.

use super::{MkStreamLabel, StreamLabel};

/// A [`MkStreamLabel`] implementation for `L`-typed labels.
///
/// This is useful for situations in which a tower middleware does not need to insect the request,
/// response, or the final outcome.
pub struct MkWithLabels<L> {
    labels: L,
}

/// A [`StreamLabel`] implementation for `L`-typed labels.
///
/// This is useful for situations in which a tower middleware does not need to insect the request,
/// response, or the final outcome.
#[derive(Clone, Debug, Default)]
pub struct WithLabels<L> {
    labels: L,
}

// === impl MkWithLabels ===

impl<L> MkStreamLabel for MkWithLabels<L>
where
    L: Clone + Send + 'static,
{
    type DurationLabels = L;
    type StatusLabels = L;
    type StreamLabel = WithLabels<L>;
    fn mk_stream_labeler<B>(&self, _: &http::Request<B>) -> Option<Self::StreamLabel> {
        Some(WithLabels {
            labels: self.labels.clone(),
        })
    }
}

// === impl WithLabels ===

impl<L> StreamLabel for WithLabels<L>
where
    L: Clone + Send + 'static,
{
    type DurationLabels = L;
    type StatusLabels = L;

    fn init_response<B>(&mut self, _: &http::Response<B>) {}
    fn end_response(&mut self, _: Result<Option<&http::HeaderMap>, &linkerd_error::Error>) {}

    fn status_labels(&self) -> Self::StatusLabels {
        self.labels.clone()
    }

    fn duration_labels(&self) -> Self::DurationLabels {
        self.labels.clone()
    }
}

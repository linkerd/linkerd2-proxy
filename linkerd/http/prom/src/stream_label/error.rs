//! [`StreamLabel`] implementation for labeling errors.

use super::StreamLabel;
use linkerd_error::Error;

/// A [`StreamLabel`] implementation that maps boxed errors to labels.
#[derive(Clone, Debug, Default)]
pub struct LabelError<E> {
    error: Option<E>,
}

// === impl LabelError ===

impl<E> StreamLabel for LabelError<E>
where
    E: for<'a> From<&'a Error>,
    E: Clone + Send + 'static,
{
    type DurationLabels = ();
    type StatusLabels = Option<E>;

    fn init_response<B>(&mut self, _: &http::Response<B>) {}

    fn end_response(&mut self, res: Result<Option<&http::HeaderMap>, &Error>) {
        let Err(err) = res else { return };
        let labels = E::from(err);
        self.error = Some(labels);
    }

    fn status_labels(&self) -> Self::StatusLabels {
        self.error.clone()
    }

    fn duration_labels(&self) -> Self::DurationLabels {}
}

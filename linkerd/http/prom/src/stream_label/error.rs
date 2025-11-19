//! [`StreamLabel`] implementation for labeling errors.

use super::StreamLabel;
use linkerd_http_body_eos::EosRef;

/// A [`StreamLabel`] implementation that maps boxed errors to labels.
#[derive(Clone, Debug, Default)]
pub struct LabelError<E> {
    error: Option<E>,
}

// === impl LabelError ===

impl<E> StreamLabel for LabelError<E>
where
    E: for<'a> From<EosRef<'a>>,
    E: Clone + Send + 'static,
{
    type DurationLabels = ();
    type StatusLabels = Option<E>;

    fn init_response<B>(&mut self, _: &http::Response<B>) {}

    fn end_response(&mut self, eos: EosRef<'_>) {
        // XXX(kate): this also needs to account for cancelled streams!
        let labels = E::from(eos);
        self.error = Some(labels);
    }

    fn status_labels(&self) -> Self::StatusLabels {
        self.error.clone()
    }

    fn duration_labels(&self) -> Self::DurationLabels {}
}

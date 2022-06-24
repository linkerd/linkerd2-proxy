#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RespondWithError {
    pub status: http::StatusCode,
    pub message: std::sync::Arc<str>,
}

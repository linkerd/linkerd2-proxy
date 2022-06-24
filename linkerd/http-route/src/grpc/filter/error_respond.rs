#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RespondWithError {
    pub code: u16,
    pub message: std::sync::Arc<str>,
}

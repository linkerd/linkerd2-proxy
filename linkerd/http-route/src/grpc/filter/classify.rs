use std::collections::BTreeSet;
use tonic::Code;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Classify {
    /// Range of gRPC status codes that are considered errors.
    pub error_codes: BTreeSet<u16>,
}

static DEFAULT_CODES: &[Code] = &[
    Code::Unknown,
    Code::DeadlineExceeded,
    Code::PermissionDenied,
    Code::Internal,
    Code::Unavailable,
    Code::DataLoss,
];

impl Default for Classify {
    fn default() -> Self {
        Self {
            error_codes: DEFAULT_CODES.iter().map(|c| *c as u16).collect(),
        }
    }
}

impl Classify {
    pub fn is_error(&self, code: Code) -> bool {
        self.error_codes.contains(&(code as u16))
    }
}

use std::collections::BTreeSet;
use tonic::Code;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Classify {}

impl Classify {
    pub fn is_error(&self, code: Code) -> bool {
        self.error_codes.contains(&(code as u16))
    }
}

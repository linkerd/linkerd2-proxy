use std::ops::RangeInclusive;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Classify {
    /// Range of HTTP status codes that are considered errors.
    pub error_ranges: Vec<RangeInclusive<u16>>,
}

impl Default for Classify {
    fn default() -> Self {
        Self {
            error_ranges: vec![500..=599],
        }
    }
}

impl Classify {
    pub fn is_error(&self, code: http::StatusCode) -> bool {
        for range in &self.error_ranges {
            if range.contains(&code.as_u16()) {
                return true;
            }
        }
        false
    }
}

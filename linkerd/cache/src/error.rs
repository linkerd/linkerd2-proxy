use std::fmt;

#[derive(Debug)]
pub struct NoCapacity(pub usize);

impl fmt::Display for NoCapacity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "router capacity reached ({})", self.0)
    }
}

impl std::error::Error for NoCapacity {}

use std::fmt;

pub(crate) type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug)]
pub struct NoCapacity(pub usize);

#[derive(Debug)]
pub struct NotRecognized;

#[derive(Debug)]
pub struct RouteUnavailable;

impl fmt::Display for NoCapacity {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "router capacity reached ({})", self.0)
    }
}

impl std::error::Error for NoCapacity {}

impl fmt::Display for NotRecognized {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.pad("route not recognized")
    }
}

impl std::error::Error for NotRecognized {}

impl fmt::Display for RouteUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "route is not ready to receive requests")
    }
}

impl std::error::Error for RouteUnavailable {}

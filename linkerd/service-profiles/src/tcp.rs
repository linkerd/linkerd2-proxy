use crate::Backends;
use std::sync::Arc;

// TODO(eliza): eventually we'll actually have to match TCP routes.
pub type RequestMatch = ();
pub type RouteSet = Arc<[(RequestMatch, Route)]>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Route {
    backends: Backends,
}

impl Route {
    #[must_use]
    pub fn new(backends: Backends) -> Self {
        Self { backends }
    }

    pub fn backends(&self) -> &Backends {
        &self.backends
    }
}

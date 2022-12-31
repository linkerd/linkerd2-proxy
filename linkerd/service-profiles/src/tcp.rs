use crate::Targets;
use std::sync::Arc;

// TODO(eliza): eventually we'll actually have to match TCP routes.
pub type RequestMatch = ();
pub type RouteSet = Arc<[(RequestMatch, Route)]>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Route {
    targets: Targets,
}

impl Route {
    #[must_use]
    pub fn new(targets: Targets) -> Self {
        Self { targets }
    }

    pub fn targets(&self) -> &Targets {
        &self.targets
    }
}

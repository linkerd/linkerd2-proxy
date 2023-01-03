use crate::Targets;

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

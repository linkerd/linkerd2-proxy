use crate::RoutePolicy;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Opaque {
    pub policy: Option<RoutePolicy<()>>,
}

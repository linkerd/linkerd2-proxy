use crate::RoutePolicy;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Opaque {
    pub policy: Option<Policy>,
}

pub type Policy = RoutePolicy<Filter>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {}

pub type Policy<D> = crate::RoutePolicy<Filter, D>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {}

use crate::RoutePolicy;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Opaque {
    pub policy: Option<Policy>,
}

pub type Policy = RoutePolicy<Filter>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {}

#[cfg(feature = "proto")]
pub(crate) mod proto {
    use super::*;

    use once_cell::sync::Lazy;
    use std::sync::Arc;

    pub(crate) static NO_FILTERS: Lazy<Arc<[Filter]>> = Lazy::new(|| Arc::new([]));
}

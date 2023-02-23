use crate::{Meta, RoutePolicy};
use once_cell::sync::Lazy;
use std::{borrow::Cow, sync::Arc};

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Opaque {
    pub policy: Option<Policy>,
}

pub type Policy = RoutePolicy<Filter>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {}

impl Opaque {
    pub(crate) fn default_policy() -> Self {
        static META: Lazy<Arc<crate::Meta>> = Lazy::new(|| {
            Arc::new(Meta::Default {
                name: Cow::Borrowed("opaque"),
            })
        });
        todo!("eliza")
    }
}

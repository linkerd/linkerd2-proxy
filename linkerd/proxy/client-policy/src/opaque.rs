use linkerd_proxy_core::Meta;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Opaque {
    pub route: Arc<Meta>,
}

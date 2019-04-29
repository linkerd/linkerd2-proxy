pub extern crate linkerd2_stack as stack;
pub extern crate linkerd2_timeout;

pub use self::linkerd2_timeout::stack as timeout;
pub use self::stack::{layer, shared, Layer, LayerExt};
pub use tower::builder::ServiceBuilder;
pub use tower::util::{Either, Oneshot};
pub use tower::{service_fn as mk, MakeConnection, MakeService, Service, ServiceExt};

pub fn builder() -> ServiceBuilder<tower::layer::util::Identity> {
    ServiceBuilder::new()
}

pub extern crate linkerd2_stack as stack;
pub extern crate linkerd2_timeout;
extern crate tower_service;
extern crate tower_service_util;

pub use self::tower_service::Service;
pub use self::tower_service_util::MakeService;

pub use self::stack::{shared, stack_per_request, watch, Either, Layer, Stack};

pub use self::linkerd2_timeout::stack as timeout;

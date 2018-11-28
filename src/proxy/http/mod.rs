pub mod balance;
pub mod client;
pub(super) mod glue;
pub mod h1;
pub mod h2;
pub mod header_from_target;
pub mod insert_target;
pub mod metrics;
pub mod normalize_uri;
pub mod orig_proto;
pub mod profiles;
pub mod router;
pub mod settings;
pub mod upgrade;

pub use self::client::Client;
pub use self::glue::{Error, HttpBody as Body, GrpcBody};
pub use self::settings::Settings;

use svc::Either;

pub trait HasH2Reason {
    fn h2_reason(&self) -> Option<::h2::Reason>;
}

impl HasH2Reason for ::h2::Error {
    fn h2_reason(&self) -> Option<::h2::Reason> {
        self.reason()
    }
}

impl<E: HasH2Reason> HasH2Reason for super::buffer::ServiceError<E> {
    fn h2_reason(&self) -> Option<::h2::Reason> {
        match self {
            super::buffer::ServiceError::Inner(e) => e.h2_reason(),
            super::buffer::ServiceError::Closed => None,
        }
    }
}

impl<A: HasH2Reason, B: HasH2Reason> HasH2Reason for Either<A, B> {
    fn h2_reason(&self) -> Option<::h2::Reason> {
        match self {
            Either::A(a) => a.h2_reason(),
            Either::B(b) => b.h2_reason(),
        }
    }
}

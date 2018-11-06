pub mod balance;
pub mod classify;
pub mod client;
pub(super) mod glue;
pub mod h1;
pub mod insert_target;
pub mod metrics;
pub mod normalize_uri;
pub mod orig_proto;
pub mod profiles;
pub mod router;
pub mod settings;
pub mod upgrade;

pub use self::classify::{Classify, ClassifyResponse};
pub use self::client::{Client, Error as ClientError};
pub use self::glue::HttpBody as Body;
pub use self::settings::Settings;

pub trait HasH2Reason {
    fn h2_reason(&self) -> Option<::h2::Reason>;
}

impl<E: HasH2Reason> HasH2Reason for super::buffer::ServiceError<E> {
    fn h2_reason(&self) -> Option<::h2::Reason> {
        match self {
            super::buffer::ServiceError::Inner(e) => e.h2_reason(),
            super::buffer::ServiceError::Closed => None,
        }
    }
}

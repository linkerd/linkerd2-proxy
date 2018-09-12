pub mod client;
pub mod dialect;
pub mod h1;
pub mod normalize_uri;
pub mod orig_proto;
pub mod router;
pub mod upgrade;

pub use self::client::{Client, Error as ClientError};
pub use self::dialect::Dialect;
pub use self::normalize_uri::NormalizeUri;
pub use super::glue::HttpBody as Body;

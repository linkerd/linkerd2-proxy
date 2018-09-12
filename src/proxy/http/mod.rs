pub mod client;
pub mod dialect;
pub(super) mod glue;
pub mod h1;
pub mod normalize_uri;
pub mod orig_proto;
pub mod router;
pub mod upgrade;

pub use self::client::{Client, Error as ClientError};
pub use self::dialect::Dialect;
pub use self::glue::HttpBody as Body;
pub use self::normalize_uri::NormalizeUri;

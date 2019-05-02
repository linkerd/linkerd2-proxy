pub mod add_header;
pub mod balance;
pub mod canonicalize;
pub mod client;
pub(super) mod glue;
pub mod h1;
pub mod h2;
pub mod header_from_target;
pub mod insert;
pub mod metrics;
pub mod normalize_uri;
pub mod orig_proto;
pub mod profiles;
pub mod retry;
pub mod router;
pub mod settings;
pub mod strip_header;
pub mod timeout;
pub mod upgrade;

pub use self::client::Client;
pub use self::glue::{ClientUsedTls, HttpBody as Body, HyperServerSvc};
pub use self::settings::Settings;

use http::header::AsHeaderName;
use http::uri::Authority;

pub trait HasH2Reason {
    fn h2_reason(&self) -> Option<::h2::Reason>;
}

impl<'a> HasH2Reason for &'a (dyn std::error::Error + 'static) {
    fn h2_reason(&self) -> Option<::h2::Reason> {
        let mut cause = Some(*self);

        while let Some(err) = cause {
            if let Some(err) = err.downcast_ref::<::h2::Error>() {
                return err.h2_reason();
            }

            cause = err.source();
        }

        None
    }
}

impl HasH2Reason for ::proxy::Error {
    fn h2_reason(&self) -> Option<::h2::Reason> {
        (&**self as &(dyn std::error::Error + 'static)).h2_reason()
    }
}

impl HasH2Reason for ::h2::Error {
    fn h2_reason(&self) -> Option<::h2::Reason> {
        self.reason()
    }
}

/// Returns an Authority from the value of `header`.
pub fn authority_from_header<B, K>(req: &http::Request<B>, header: K) -> Option<Authority>
where
    K: AsHeaderName,
{
    req.headers().get(header).and_then(|value| {
        value.to_str().ok().and_then(|s| {
            if s.is_empty() {
                None
            } else {
                s.parse::<Authority>().ok()
            }
        })
    })
}
